//! The command line, as clap definitions. Every command and flag here is
//! described in `docs/cli.md`.

use clap::{Args, Parser, Subcommand, ValueEnum};
use std::net::SocketAddr;
use std::path::PathBuf;

/// Builderella: one build-and-test pool from the computers you have.
#[derive(Debug, Parser)]
#[command(name = "bella", version, disable_help_subcommand = true)]
pub struct Cli {
    /// Print one JSON document on standard output instead of text.
    #[arg(long, global = true)]
    pub json: bool,

    /// Credentials file to use instead of the per-user default.
    #[arg(long, global = true, env = "BELLA_CREDENTIALS", value_name = "PATH")]
    pub credentials: Option<PathBuf>,

    /// More diagnostics on standard error (repeat for more). `RUST_LOG`
    /// overrides it.
    #[arg(short, long, global = true, action = clap::ArgAction::Count)]
    pub verbose: u8,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Run and administer the controller.
    #[command(subcommand)]
    Controller(ControllerCmd),
    /// Run and control the worker on this machine.
    #[command(subcommand)]
    Worker(WorkerCmd),
    /// Store credentials for a controller after checking them.
    Login(LoginArgs),
    /// Mint an access token (administrators).
    #[command(subcommand)]
    Token(TokenCmd),
    /// Snapshot the working tree and submit a job.
    Submit(SubmitArgs),
    /// Dry run: could the fleet run this job, and where?
    Plan(PlanArgs),
    /// List jobs.
    Jobs(JobsArgs),
    /// Show one job.
    Job(JobIdArg),
    /// Wait until a job has finished.
    Wait(WaitArgs),
    /// Print a job's log output.
    Logs(LogsArgs),
    /// Ask the fleet to stop a job.
    Cancel(JobIdArg),
    /// List or download a job's artifacts.
    Artifacts(ArtifactsArgs),
    /// Why a job is waiting.
    Explain(JobIdArg),
    /// Environments kept alive after a job.
    #[command(subcommand)]
    Env(EnvCmd),
    /// Machines, their state and capacity.
    Fleet,
    /// Queued jobs and fair-share standing.
    Queue,
    /// Inspect and administer one machine.
    #[command(subcommand)]
    Machine(MachineCmd),
    /// Projects and their policy.
    #[command(subcommand)]
    Project(ProjectCmd),
    /// Serve the fleet to a coding agent over MCP on standard input and output.
    Mcp(McpArgs),
    /// Versions of this binary and of the controller.
    Version,
}

// ------------------------------------------------------------- controller

#[derive(Debug, Subcommand)]
pub enum ControllerCmd {
    /// Run the controller in the foreground.
    Run(ControllerRunArgs),
    /// Create a pairing code for a new machine (administrators).
    Pair(PairArgs),
    /// Install or manage the controller as an operating-system service.
    #[command(subcommand)]
    Service(ControllerServiceCmd),
}

#[derive(Clone, Debug, Args)]
pub struct ControllerRunArgs {
    /// State directory: ledger, objects, certificate, admin token.
    #[arg(long, value_name = "DIR")]
    pub data_dir: PathBuf,
    /// Address to listen on. Port 0 picks a free port.
    #[arg(long, value_name = "ADDR", default_value = "0.0.0.0:7420")]
    pub listen: SocketAddr,
    /// Address workers and clients use, `https://host:port`. Needed when
    /// listening on a wildcard address.
    #[arg(long, value_name = "URL")]
    pub public_url: Option<String>,
    /// Extra names or addresses for the fleet certificate. Only used when the
    /// certificate is first generated.
    #[arg(long = "san", value_name = "NAME")]
    pub subject_alt_names: Vec<String>,
    /// Serve plain HTTP. Tokens and source then cross the network
    /// unencrypted; refused on anything but loopback without
    /// `--allow-insecure`.
    #[arg(long)]
    pub insecure_http: bool,
    /// With `--insecure-http`, permit an address other than loopback.
    #[arg(long, requires = "insecure_http")]
    pub allow_insecure: bool,
    /// Write `{"url", "fingerprint"}` as JSON to this file once listening.
    #[arg(long, value_name = "FILE")]
    pub ready_file: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub struct PairArgs {
    /// How long the code stays valid, such as `10m`.
    #[arg(long, value_name = "DURATION", default_value = "10m")]
    pub ttl: String,
    /// What the new machine is for.
    #[arg(long, value_enum, default_value_t = Profile::ProtectedWorkstation)]
    pub profile: Profile,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum Profile {
    ProtectedWorkstation,
    DedicatedBuilder,
    PortableBuilder,
    InfrastructureOnly,
}

macro_rules! service_commands {
    ($cmd:ident, $install:ident, $run:ty) => {
        /// `service install|uninstall|status|start|stop`.
        #[derive(Debug, Subcommand)]
        pub enum $cmd {
            /// Write the definition and start the service.
            Install(Box<$install>),
            /// Stop and remove the service.
            Uninstall(ServiceIdArgs),
            /// Report the service's state.
            Status(ServiceIdArgs),
            /// Start the installed service.
            Start(ServiceIdArgs),
            /// Stop the installed service.
            Stop(ServiceIdArgs),
        }

        /// The flags of `run`, which the service executes, plus how to
        /// install it.
        #[derive(Debug, Args)]
        pub struct $install {
            #[command(flatten)]
            pub run: $run,
            #[command(flatten)]
            pub service: ServiceIdArgs,
            #[command(flatten)]
            pub install: InstallFlags,
        }
    };
}

service_commands!(
    ControllerServiceCmd,
    ControllerServiceInstall,
    ControllerRunArgs
);
service_commands!(WorkerServiceCmd, WorkerServiceInstall, WorkerRunArgs);

#[derive(Debug, Args)]
pub struct InstallFlags {
    /// Show the definition that would be written and change nothing.
    #[arg(long)]
    pub print: bool,
    /// Account the service runs as. Workers on Windows name an ordinary
    /// account; its password is asked for.
    #[arg(long, value_name = "ACCOUNT")]
    pub account: Option<String>,
    /// Directory for the service's log files.
    #[arg(long, value_name = "DIR")]
    pub log_dir: Option<PathBuf>,
    /// Replace a different definition installed earlier by bella.
    #[arg(long)]
    pub replace: bool,
    /// Install without starting.
    #[arg(long)]
    pub no_start: bool,
}

#[derive(Debug, Args)]
pub struct ServiceIdArgs {
    /// Install for the current user instead of the whole system (Linux and
    /// macOS).
    #[arg(long)]
    pub user: bool,
    /// Service name instead of the default.
    #[arg(long, value_name = "NAME")]
    pub name: Option<String>,
}

// ----------------------------------------------------------------- worker

#[derive(Debug, Subcommand)]
pub enum WorkerCmd {
    /// Join this machine to a fleet with an enrollment string.
    Enroll(EnrollArgs),
    /// Run the worker daemon in the foreground.
    Run(WorkerRunArgs),
    /// Accept no new work; running work continues.
    Pause(DataDirArg),
    /// Accept new work again after `pause`.
    Resume(DataDirArg),
    /// Accept no new work and let the machine run empty.
    Drain(DataDirArg),
    /// End `drain`.
    Undrain(DataDirArg),
    /// Allow managed work the static ceiling for a while.
    Boost(BoostArgs),
    /// Local pause, drain and boost settings and the enrollment.
    Status(DataDirArg),
    /// Check containment and run one command the way a step runs.
    SelfTest(SelfTestArgs),
    /// Install or manage the worker as an operating-system service.
    #[command(subcommand)]
    Service(WorkerServiceCmd),
}

#[derive(Debug, Args)]
pub struct EnrollArgs {
    /// The `bella://…` string printed by `bella controller pair`.
    pub enrollment: String,
    #[arg(long, value_name = "DIR")]
    pub data_dir: PathBuf,
    /// Display name; defaults to the host name.
    #[arg(long, value_name = "NAME")]
    pub name: Option<String>,
    /// Accept a `bella+http://` string naming a host other than loopback.
    #[arg(long)]
    pub allow_insecure: bool,
}

#[derive(Clone, Debug, Args)]
pub struct WorkerRunArgs {
    #[arg(long, value_name = "DIR")]
    pub data_dir: PathBuf,
    /// Where attempt workspaces are created. Keep it short on Windows.
    #[arg(long, value_name = "DIR")]
    pub work_root: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub struct DataDirArg {
    #[arg(long, value_name = "DIR")]
    pub data_dir: PathBuf,
}

#[derive(Debug, Args)]
pub struct BoostArgs {
    #[arg(long, value_name = "DIR")]
    pub data_dir: PathBuf,
    /// How long the boost lasts.
    #[arg(long = "for", value_name = "DURATION", default_value = "30m")]
    pub duration: String,
    /// End a boost now.
    #[arg(long, conflicts_with = "duration")]
    pub off: bool,
}

#[derive(Debug, Args)]
pub struct SelfTestArgs {
    #[arg(long, value_name = "DIR")]
    pub data_dir: PathBuf,
    /// Where the test command's scratch directory is made.
    #[arg(long, value_name = "DIR")]
    pub work_root: Option<PathBuf>,
}

// ------------------------------------------------------------- identity

#[derive(Debug, Args)]
pub struct LoginArgs {
    /// Controller address, `https://host:port`.
    #[arg(required_unless_present = "local")]
    pub url: Option<String>,
    /// The controller's certificate fingerprint, as printed by
    /// `bella controller run`.
    #[arg(long, value_name = "FP")]
    pub fingerprint: Option<String>,
    /// Read the token from standard input.
    #[arg(long, conflicts_with = "token_file")]
    pub token_stdin: bool,
    /// Read the token from this file.
    #[arg(long, value_name = "FILE")]
    pub token_file: Option<PathBuf>,
    /// On the controller host: use the admin token and certificate in its
    /// data directory.
    #[arg(long, requires = "data_dir", conflicts_with_all = ["url", "fingerprint", "token_stdin", "token_file"])]
    pub local: bool,
    #[arg(long, value_name = "DIR")]
    pub data_dir: Option<PathBuf>,
    /// With `--local`: the address to reach the controller at, when it does
    /// not listen on loopback.
    #[arg(long, value_name = "URL", requires = "local")]
    pub controller_url: Option<String>,
    /// Accept a plain `http://` address other than loopback.
    #[arg(long)]
    pub allow_insecure: bool,
}

#[derive(Debug, Subcommand)]
pub enum TokenCmd {
    /// Mint a token; it is printed once.
    Create(TokenCreateArgs),
}

#[derive(Debug, Args)]
pub struct TokenCreateArgs {
    #[arg(long, value_enum)]
    pub role: RoleArg,
    #[arg(long, value_name = "NAME")]
    pub name: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum RoleArg {
    Viewer,
    Developer,
    Admin,
}

// ------------------------------------------------------------------ jobs

/// What to run: a recipe or a command, with per-job overrides.
#[derive(Clone, Debug, Default, Args)]
pub struct WorkArgs {
    /// Recipe name from `builderella.toml`. With exactly one recipe in the
    /// file it may be left out.
    pub recipe: Option<String>,
    /// Project name on the fleet; defaults to the recipe file's.
    #[arg(long, value_name = "NAME")]
    pub project: Option<String>,
    /// Directory to work in instead of the current one.
    #[arg(long, short = 'C', value_name = "DIR")]
    pub dir: Option<PathBuf>,
    #[arg(long, value_enum)]
    pub priority: Option<PriorityArg>,
    /// Preferred platform, such as `linux` or `macos-aarch64`.
    #[arg(long, value_name = "PLATFORM", conflicts_with = "machine")]
    pub platform: Option<String>,
    /// With `--platform`: never run anywhere else.
    #[arg(long, requires = "platform")]
    pub require: bool,
    /// Run only on this machine (id or name).
    #[arg(long, value_name = "MACHINE")]
    pub machine: Option<String>,
    /// Extra environment variable for the steps, `KEY=VALUE`.
    #[arg(long = "env", value_name = "KEY=VALUE")]
    pub env: Vec<String>,
    /// Job timeout, such as `20m`.
    #[arg(long, value_name = "DURATION")]
    pub timeout: Option<String>,
    /// Keep the source on the local network.
    #[arg(long)]
    pub lan_only: bool,
    /// With a command: CPUs to reserve (default 1).
    #[arg(long, value_name = "CPUS")]
    pub cpu: Option<String>,
    /// With a command: memory to reserve, such as `2GiB` (default 1GiB).
    #[arg(long, value_name = "SIZE")]
    pub memory: Option<String>,
    /// Run this command instead of a recipe; no shell is involved.
    #[arg(last = true, value_name = "COMMAND")]
    pub command: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum PriorityArg {
    Maintenance,
    Background,
    Normal,
    Interactive,
    Urgent,
}

#[derive(Debug, Args)]
pub struct SubmitArgs {
    #[command(flatten)]
    pub work: WorkArgs,
    /// Wait until the job has finished; the exit status follows its outcome.
    #[arg(long)]
    pub wait: bool,
    /// Wait and print the log as it arrives.
    #[arg(long)]
    pub follow: bool,
    /// With `--wait`: stop waiting after this long (the job keeps running).
    #[arg(long, value_name = "DURATION")]
    pub wait_timeout: Option<String>,
    /// Reuse the key of an earlier submission to get the same job back.
    #[arg(long, value_name = "KEY")]
    pub idempotency_key: Option<String>,
}

#[derive(Debug, Args)]
pub struct PlanArgs {
    #[command(flatten)]
    pub work: WorkArgs,
}

#[derive(Debug, Args)]
pub struct JobsArgs {
    #[arg(long, value_name = "NAME")]
    pub project: Option<String>,
    /// `active` (not finished), `finished`, or a phase such as `queued`.
    #[arg(long, value_name = "STATE")]
    pub state: Option<String>,
    #[arg(long, value_name = "N", default_value_t = 50)]
    pub limit: u32,
}

#[derive(Debug, Args)]
pub struct JobIdArg {
    pub id: String,
}

#[derive(Debug, Args)]
pub struct WaitArgs {
    pub id: String,
    /// Stop waiting after this long (the job keeps running).
    #[arg(long, value_name = "DURATION")]
    pub timeout: Option<String>,
}

#[derive(Debug, Args)]
pub struct LogsArgs {
    pub id: String,
    /// Keep printing until the job's output is complete.
    #[arg(long, short = 'f')]
    pub follow: bool,
    /// Only this step.
    #[arg(long, value_name = "STEP")]
    pub step: Option<String>,
    /// Only the last part of the output, about this many bytes.
    #[arg(long, value_name = "SIZE")]
    pub tail: Option<String>,
}

#[derive(Debug, Args)]
pub struct ArtifactsArgs {
    pub id: String,
    /// Download every artifact into this directory. Existing files are
    /// never overwritten.
    #[arg(long, value_name = "DIR")]
    pub download: Option<PathBuf>,
}

#[derive(Debug, Subcommand)]
pub enum EnvCmd {
    List,
    Show(EnvIdArg),
    /// Extend the lease.
    Renew(EnvIdArg),
    /// Tear the environment down.
    Finish(EnvIdArg),
}

#[derive(Debug, Args)]
pub struct EnvIdArg {
    pub id: String,
}

// ------------------------------------------------------------ admin views

#[derive(Debug, Subcommand)]
pub enum MachineCmd {
    Show(MachineIdArg),
    Rename(MachineRenameArgs),
    /// Offer no new work to the machine.
    Drain(MachineIdArg),
    Undrain(MachineIdArg),
    /// Stop offering work and revoke the machine's credentials.
    Retire(MachineIdArg),
    /// Change the machine's policy.
    Policy(MachinePolicyArgs),
}

#[derive(Debug, Args)]
pub struct MachineIdArg {
    /// Machine id or unique name.
    pub machine: String,
}

#[derive(Debug, Args)]
pub struct MachineRenameArgs {
    pub machine: String,
    pub name: String,
}

#[derive(Debug, Args)]
pub struct MachinePolicyArgs {
    pub machine: String,
    #[arg(long, value_enum)]
    pub profile: Option<Profile>,
    #[arg(long, value_name = "BOOL")]
    pub accept_builds: Option<bool>,
    /// Share of CPU withheld for the foreground, in percent.
    #[arg(long, value_name = "PERCENT")]
    pub foreground_cpu: Option<u8>,
    /// Share of memory withheld for the foreground, in percent.
    #[arg(long, value_name = "PERCENT")]
    pub foreground_memory: Option<u8>,
    /// Attempts allowed at once; `0` removes the limit.
    #[arg(long, value_name = "N")]
    pub max_concurrent: Option<u32>,
    #[arg(long, value_name = "BOOL")]
    pub low_priority: Option<bool>,
    /// Longest an attempt may run without controller contact.
    #[arg(long, value_name = "DURATION")]
    pub max_disconnected: Option<String>,
}

#[derive(Debug, Subcommand)]
pub enum ProjectCmd {
    List,
    Show(ProjectNameArg),
    /// Change a project's policy (administrators).
    Set(ProjectSetArgs),
}

#[derive(Debug, Args)]
pub struct ProjectNameArg {
    pub name: String,
}

#[derive(Debug, Args)]
pub struct ProjectSetArgs {
    pub name: String,
    #[arg(long, value_name = "N")]
    pub weight: Option<u32>,
    /// Attempts allowed at once; `0` removes the limit.
    #[arg(long, value_name = "N")]
    pub max_concurrent: Option<u32>,
    /// Environments allowed at once; `0` removes the limit.
    #[arg(long, value_name = "N")]
    pub max_environments: Option<u32>,
    #[arg(long, value_enum)]
    pub cloud: Option<CloudArg>,
    #[arg(long, value_name = "BOOL")]
    pub lan_only: Option<bool>,
    #[arg(long, value_name = "BOOL")]
    pub allow_urgent: Option<bool>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum CloudArg {
    ExistingOnly,
    Manual,
    AutoOverflow,
    Hybrid,
}

#[derive(Debug, Args)]
pub struct McpArgs {
    /// A directory the agent may snapshot or download into. Repeat for
    /// several; the first is the default workspace.
    #[arg(long = "root", value_name = "DIR")]
    pub roots: Vec<PathBuf>,
    /// Longest one `wait_job` call blocks, such as `50s`.
    #[arg(long, value_name = "DURATION")]
    pub max_wait: Option<String>,
}
