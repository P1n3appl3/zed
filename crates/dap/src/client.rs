use crate::transport::{Events, Payload, Request, Response, Transport};
use anyhow::{anyhow, Context, Result};

use dap_types::{
    requests::{
        Attach, ConfigurationDone, Continue, Disconnect, Initialize, Launch, Next, Pause, Restart,
        SetBreakpoints, StepBack, StepIn, StepOut,
    },
    AttachRequestArguments, ConfigurationDoneArguments, ContinueArguments, ContinueResponse,
    DisconnectArguments, InitializeRequestArgumentsPathFormat, LaunchRequestArguments,
    NextArguments, PauseArguments, RestartArguments, Scope, SetBreakpointsArguments,
    SetBreakpointsResponse, Source, SourceBreakpoint, StackFrame, StepBackArguments,
    StepInArguments, StepOutArguments, SteppingGranularity, Variable,
};
use futures::{AsyncBufRead, AsyncReadExt, AsyncWrite};
use gpui::{AppContext, AsyncAppContext};
use parking_lot::{Mutex, MutexGuard};
use serde_json::Value;
use smol::{
    channel::{bounded, unbounded, Receiver, Sender},
    io::BufReader,
    net::{TcpListener, TcpStream},
    process::{self, Child},
};
use std::{
    collections::HashMap,
    net::{Ipv4Addr, SocketAddrV4},
    path::PathBuf,
    process::Stdio,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};
use task::{DebugAdapterConfig, DebugConnectionType, DebugRequestType, TCPHost};
use util::ResultExt;

#[derive(Copy, Clone, Default, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ThreadStatus {
    #[default]
    Running,
    Stopped,
    Exited,
    Ended,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub struct DebugAdapterClientId(pub usize);

#[derive(Debug, Default, Clone)]
pub struct ThreadState {
    pub status: ThreadStatus,
    pub stack_frames: Vec<StackFrame>,
    pub scopes: HashMap<u64, Vec<Scope>>, // stack_frame_id -> scopes
    pub variables: HashMap<u64, Vec<Variable>>, // scope.variable_reference -> variables
    pub current_stack_frame_id: Option<u64>,
}

pub struct DebugAdapterClient {
    id: DebugAdapterClientId,
    _process: Option<Child>,
    server_tx: Sender<Payload>,
    request_count: AtomicU64,
    capabilities: Arc<Mutex<Option<dap_types::Capabilities>>>,
    config: DebugAdapterConfig,
    thread_states: Arc<Mutex<HashMap<u64, ThreadState>>>, // thread_id -> thread_state
}

impl DebugAdapterClient {
    /// Creates & returns a new debug adapter client
    ///
    /// # Parameters
    /// - `id`: The id that [`Project`](project::Project) uses to keep track of specific clients
    /// - `config`: The adapter specific configurations from debugger task that is starting
    /// - `command`: The command that starts the debugger
    /// - `args`: Arguments of the command that starts the debugger
    /// - `project_path`: The absolute path of the project that is being debugged
    /// - `cx`: The context that the new client belongs too
    pub async fn new<F>(
        id: DebugAdapterClientId,
        config: DebugAdapterConfig,
        command: &str,
        args: Vec<&str>,
        project_path: PathBuf,
        event_handler: F,
        cx: &mut AsyncAppContext,
    ) -> Result<Self>
    where
        F: FnMut(Events, &mut AppContext) + 'static + Send + Sync + Clone,
    {
        match config.connection.clone() {
            DebugConnectionType::TCP(host) => {
                Self::create_tcp_client(
                    id,
                    config,
                    host,
                    command,
                    args,
                    project_path,
                    event_handler,
                    cx,
                )
                .await
            }
            DebugConnectionType::STDIO => {
                Self::create_stdio_client(
                    id,
                    config,
                    command,
                    args,
                    project_path,
                    event_handler,
                    cx,
                )
                .await
            }
        }
    }

    /// Creates a debug client that connects to an adapter through tcp
    ///
    /// TCP clients don't have an error communication stream with an adapter
    ///
    /// # Parameters
    /// - `id`: The id that [`Project`](project::Project) uses to keep track of specific clients
    /// - `config`: The adapter specific configurations from debugger task that is starting
    /// - `command`: The command that starts the debugger
    /// - `args`: Arguments of the command that starts the debugger
    /// - `project_path`: The absolute path of the project that is being debugged
    /// - `cx`: The context that the new client belongs too
    #[allow(clippy::too_many_arguments)]
    async fn create_tcp_client<F>(
        id: DebugAdapterClientId,
        config: DebugAdapterConfig,
        host: TCPHost,
        command: &str,
        args: Vec<&str>,
        project_path: PathBuf,
        event_handler: F,
        cx: &mut AsyncAppContext,
    ) -> Result<Self>
    where
        F: FnMut(Events, &mut AppContext) + 'static + Send + Sync + Clone,
    {
        let mut port = host.port;
        if port.is_none() {
            port = Self::get_port().await;
        }

        let mut command = process::Command::new(command);
        command
            .current_dir(project_path)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);

        let process = command
            .spawn()
            .with_context(|| "failed to start debug adapter.")?;

        if let Some(delay) = host.delay {
            // some debug adapters need some time to start the TCP server
            // so we have to wait few milliseconds before we can connect to it
            cx.background_executor()
                .timer(Duration::from_millis(delay))
                .await;
        }

        let address = SocketAddrV4::new(
            host.host.unwrap_or_else(|| Ipv4Addr::new(127, 0, 0, 1)),
            port.unwrap(),
        );

        let (rx, tx) = TcpStream::connect(address).await?.split();

        Self::handle_transport(
            id,
            config,
            Box::new(BufReader::new(rx)),
            Box::new(tx),
            None,
            Some(process),
            event_handler,
            cx,
        )
    }

    /// Get an open port to use with the tcp client when not supplied by debug config
    async fn get_port() -> Option<u16> {
        Some(
            TcpListener::bind(SocketAddrV4::new(Ipv4Addr::new(127, 0, 0, 1), 0))
                .await
                .ok()?
                .local_addr()
                .ok()?
                .port(),
        )
    }

    /// Creates a debug client that connects to an adapter through std input/output
    ///
    /// # Parameters
    /// - `id`: The id that [`Project`](project::Project) uses to keep track of specific clients
    /// - `config`: The adapter specific configurations from debugger task that is starting
    /// - `command`: The command that starts the debugger
    /// - `args`: Arguments of the command that starts the debugger
    /// - `project_path`: The absolute path of the project that is being debugged
    /// - `cx`: The context that the new client belongs too
    async fn create_stdio_client<F>(
        id: DebugAdapterClientId,
        config: DebugAdapterConfig,
        command: &str,
        args: Vec<&str>,
        project_path: PathBuf,
        event_handler: F,
        cx: &mut AsyncAppContext,
    ) -> Result<Self>
    where
        F: FnMut(Events, &mut AppContext) + 'static + Send + Sync + Clone,
    {
        let mut command = process::Command::new(command);
        command
            .current_dir(project_path)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let mut process = command
            .spawn()
            .with_context(|| "failed to spawn command.")?;

        let stdin = process
            .stdin
            .take()
            .ok_or_else(|| anyhow!("Failed to open stdin"))?;
        let stdout = process
            .stdout
            .take()
            .ok_or_else(|| anyhow!("Failed to open stdout"))?;
        let stderr = process
            .stderr
            .take()
            .ok_or_else(|| anyhow!("Failed to open stderr"))?;

        let stdin = Box::new(stdin);
        let stdout = Box::new(BufReader::new(stdout));
        let stderr = Box::new(BufReader::new(stderr));

        Self::handle_transport(
            id,
            config,
            stdout,
            stdin,
            Some(stderr),
            Some(process),
            event_handler,
            cx,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn handle_transport<F>(
        id: DebugAdapterClientId,
        config: DebugAdapterConfig,
        rx: Box<dyn AsyncBufRead + Unpin + Send>,
        tx: Box<dyn AsyncWrite + Unpin + Send>,
        err: Option<Box<dyn AsyncBufRead + Unpin + Send>>,
        process: Option<Child>,
        event_handler: F,
        cx: &mut AsyncAppContext,
    ) -> Result<Self>
    where
        F: FnMut(Events, &mut AppContext) + 'static + Send + Sync + Clone,
    {
        let (server_rx, server_tx) = Transport::start(rx, tx, err, cx);
        let (client_tx, client_rx) = unbounded::<Payload>();

        let client = Self {
            id,
            config,
            server_tx,
            _process: process,
            request_count: AtomicU64::new(1),
            capabilities: Default::default(),
            thread_states: Arc::new(Mutex::new(HashMap::new())),
        };

        cx.update(|cx| {
            cx.background_executor()
                .spawn(Self::handle_recv(server_rx, client_tx))
                .detach_and_log_err(cx);

            cx.spawn(|mut cx| async move {
                Self::handle_events(client_rx, event_handler, &mut cx).await
            })
            .detach_and_log_err(cx);
        })?;

        Ok(client)
    }

    /// Set's up a client's event handler.
    ///
    /// This function should only be called once or else errors will arise
    /// # Parameters
    /// `client`: A pointer to the client to pass the event handler too
    /// `event_handler`: The function that is called to handle events
    ///     should be DebugPanel::handle_debug_client_events
    /// `cx`: The context that this task will run in
    pub async fn handle_events<F>(
        client_rx: Receiver<Payload>,
        mut event_handler: F,
        cx: &mut AsyncAppContext,
    ) -> Result<()>
    where
        F: FnMut(Events, &mut AppContext) + 'static + Send + Sync + Clone,
    {
        while let Ok(payload) = client_rx.recv().await {
            cx.update(|cx| match payload {
                Payload::Event(event) => event_handler(*event, cx),
                err => {
                    log::error!("Invalid Event: {:#?}", err);
                }
            })?;
        }

        anyhow::Ok(())
    }

    async fn handle_recv(server_rx: Receiver<Payload>, client_tx: Sender<Payload>) -> Result<()> {
        while let Ok(payload) = server_rx.recv().await {
            match payload {
                Payload::Event(ev) => client_tx.send(Payload::Event(ev)).await?,
                Payload::Response(_) => unreachable!(),
                Payload::Request(req) => client_tx.send(Payload::Request(req)).await?,
            };
        }

        anyhow::Ok(())
    }

    /// Send a request to an adapter and get a response back
    /// Note: This function will block until a response is sent back from the adapter
    pub async fn request<R: dap_types::requests::Request>(
        &self,
        arguments: R::Arguments,
    ) -> Result<R::Response> {
        let serialized_arguments = serde_json::to_value(arguments)?;

        let (callback_tx, callback_rx) = bounded::<Result<Response>>(1);

        let request = Request {
            back_ch: Some(callback_tx),
            seq: self.next_request_id(),
            command: R::COMMAND.to_string(),
            arguments: Some(serialized_arguments),
        };

        self.server_tx.send(Payload::Request(request)).await?;

        let response = callback_rx.recv().await??;

        match response.success {
            true => Ok(serde_json::from_value(response.body.unwrap_or_default())?),
            false => Err(anyhow!("Request failed")),
        }
    }

    pub fn id(&self) -> DebugAdapterClientId {
        self.id
    }

    pub fn config(&self) -> DebugAdapterConfig {
        self.config.clone()
    }

    pub fn request_type(&self) -> DebugRequestType {
        self.config.request.clone()
    }

    pub fn capabilities(&self) -> dap_types::Capabilities {
        self.capabilities.lock().clone().unwrap_or_default()
    }

    pub fn next_request_id(&self) -> u64 {
        self.request_count.fetch_add(1, Ordering::Relaxed)
    }

    pub fn update_thread_state_status(&self, thread_id: u64, status: ThreadStatus) {
        if let Some(thread_state) = self.thread_states().get_mut(&thread_id) {
            thread_state.status = status;
        };
    }

    pub fn thread_states(&self) -> MutexGuard<HashMap<u64, ThreadState>> {
        self.thread_states.lock()
    }

    pub fn thread_state_by_id(&self, thread_id: u64) -> ThreadState {
        self.thread_states.lock().get(&thread_id).cloned().unwrap()
    }

    pub async fn initialize(&self) -> Result<dap_types::Capabilities> {
        let args = dap_types::InitializeRequestArguments {
            client_id: Some("zed".to_owned()),
            client_name: Some("Zed".to_owned()),
            adapter_id: self.config.id.clone(),
            locale: Some("en-us".to_owned()),
            path_format: Some(InitializeRequestArgumentsPathFormat::Path),
            supports_variable_type: Some(true),
            supports_variable_paging: Some(false),
            supports_run_in_terminal_request: Some(false), // TODO: we should support this
            supports_memory_references: Some(true),
            supports_progress_reporting: Some(true),
            supports_invalidated_event: Some(false),
            lines_start_at1: Some(true),
            columns_start_at1: Some(true),
            supports_memory_event: Some(true),
            supports_args_can_be_interpreted_by_shell: None,
            supports_start_debugging_request: Some(true),
        };

        let capabilities = self.request::<Initialize>(args).await?;

        *self.capabilities.lock() = Some(capabilities.clone());

        Ok(capabilities)
    }

    pub async fn launch(&self, args: Option<Value>) -> Result<()> {
        self.request::<Launch>(LaunchRequestArguments {
            raw: args.unwrap_or(Value::Null),
        })
        .await
    }

    pub async fn attach(&self, args: Option<Value>) -> Result<()> {
        self.request::<Attach>(AttachRequestArguments {
            raw: args.unwrap_or(Value::Null),
        })
        .await
    }

    pub async fn resume(&self, thread_id: u64) -> Result<ContinueResponse> {
        self.request::<Continue>(ContinueArguments {
            thread_id,
            single_thread: Some(true),
        })
        .await
    }

    pub async fn step_over(&self, thread_id: u64) -> Result<()> {
        self.request::<Next>(NextArguments {
            thread_id,
            granularity: Some(SteppingGranularity::Statement),
            single_thread: Some(true),
        })
        .await
    }

    pub async fn step_in(&self, thread_id: u64) -> Result<()> {
        self.request::<StepIn>(StepInArguments {
            thread_id,
            target_id: None,
            granularity: Some(SteppingGranularity::Statement),
            single_thread: Some(true),
        })
        .await
    }

    pub async fn step_out(&self, thread_id: u64) -> Result<()> {
        self.request::<StepOut>(StepOutArguments {
            thread_id,
            granularity: Some(SteppingGranularity::Statement),
            single_thread: Some(true),
        })
        .await
    }

    pub async fn step_back(&self, thread_id: u64) -> Result<()> {
        self.request::<StepBack>(StepBackArguments {
            thread_id,
            single_thread: Some(true),
            granularity: Some(SteppingGranularity::Statement),
        })
        .await
    }

    pub async fn restart(&self) {
        self.request::<Restart>(RestartArguments {
            raw: self
                .config
                .request_args
                .as_ref()
                .map(|v| v.args.clone())
                .unwrap_or(Value::Null),
        })
        .await
        .log_err();
    }

    pub async fn pause(&self, thread_id: u64) {
        self.request::<Pause>(PauseArguments { thread_id })
            .await
            .log_err();
    }

    pub async fn stop(&self) {
        self.request::<Disconnect>(DisconnectArguments {
            restart: Some(false),
            terminate_debuggee: Some(false),
            suspend_debuggee: Some(false),
        })
        .await
        .log_err();
    }

    pub async fn set_breakpoints(
        &self,
        path: PathBuf,
        breakpoints: Option<Vec<SourceBreakpoint>>,
    ) -> Result<SetBreakpointsResponse> {
        let adapter_data = self.config.request_args.clone().map(|c| c.args);

        self.request::<SetBreakpoints>(SetBreakpointsArguments {
            source: Source {
                path: Some(String::from(path.to_string_lossy())),
                name: None,
                source_reference: None,
                presentation_hint: None,
                origin: None,
                sources: None,
                adapter_data,
                checksums: None,
            },
            breakpoints,
            source_modified: None,
            lines: None,
        })
        .await
    }

    pub async fn configuration_done(&self) -> Result<()> {
        self.request::<ConfigurationDone>(ConfigurationDoneArguments)
            .await
    }
}
