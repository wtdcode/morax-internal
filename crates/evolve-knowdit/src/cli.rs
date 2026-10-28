use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};
use evolve_knowdit_matrix::MatrixArgs;
use evolve_knowdit_report::MergeArgs;
use evolve_knowdit_verify::{ForgeArgs, VerifyArgs};
use llmy::clap::OpenAISetup;

#[derive(Parser, Debug)]
#[command(
    name = "evolve-knowdit",
    about = "Model-driven smart contract auditing with dynamically verified obligations"
)]
pub struct ProgramCommand {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Build the coverage matrix, verify every task dynamically and render the report.
    Run(Box<RunArgs>),
    /// Resolve the project and print its overview and per-dimension inventories (no LLM).
    Inspect(InspectArgs),
    /// Re-render the report from the records an earlier run persisted.
    Report(ReportArgs),
    /// Copy the project and smoke-test its forge environment (no LLM).
    Preflight(PreflightArgs),
}

#[derive(Args, Debug)]
pub struct PreflightArgs {
    #[command(flatten)]
    pub project: ProjectArgs,
    #[arg(long, short = 'o')]
    pub out: PathBuf,
    #[command(flatten)]
    pub forge: ForgeArgs,
}

#[derive(Args, Debug, Clone)]
pub struct ProjectArgs {
    /// Directory holding the project (a Foundry root or a dataset root containing one).
    #[arg(long, short = 'p')]
    pub project: PathBuf,
    /// Foundry root relative to --project when the dataset holds several.
    #[arg(long)]
    pub foundry_root: Option<PathBuf>,
    /// In-scope file list (one path per line, relative to --project); defaults to scope.txt.
    #[arg(long)]
    pub scope_file: Option<PathBuf>,
    /// Out-of-scope file list; defaults to out_of_scope.txt.
    #[arg(long)]
    pub out_of_scope_file: Option<PathBuf>,
    /// A file stating the adversary and impact the audit targets; its text replaces the
    /// built-in threat model in every agent's prompt (unset: the built-in one).
    #[arg(long)]
    pub threat_model_file: Option<PathBuf>,
    /// Characters of README and known-issues text shown to the agents.
    #[arg(long, default_value_t = 8000)]
    pub readme_chars: usize,
    /// Work in a private copy of the Foundry root under the output directory.
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    pub copy: bool,
    /// A forge-std checkout to provision into working copies that lack one.
    #[arg(long, env = "EVOLVE_FORGE_STD_SOURCE")]
    pub forge_std_source: Option<PathBuf>,
}

#[derive(ValueEnum, Debug, Clone, Copy, PartialEq, Eq)]
pub enum Until {
    Matrix,
    Verify,
    Report,
}

#[derive(Args, Debug)]
pub struct RunArgs {
    #[command(flatten)]
    pub project: ProjectArgs,
    /// Output directory; an existing one is resumed.
    #[arg(long, short = 'o')]
    pub out: PathBuf,
    /// Dimensions to model, comma separated.
    #[arg(
        long,
        value_delimiter = ',',
        default_value = "module,operation_flow,asset_lifecycle"
    )]
    pub dimensions: Vec<String>,
    /// Stop after this stage.
    #[arg(long, value_enum, default_value_t = Until::Report)]
    pub until: Until,
    /// Skip the LLM clustering of duplicate findings.
    #[arg(long, default_value_t = false)]
    pub no_merge: bool,
    /// Offer `read_module_source` (a contract's source in capped line ranges) to the agents;
    /// without it contracts are read through `read_contract_metadata`, `read_callable_source`
    /// and `read_type_definition`.
    #[arg(long, default_value_t = false)]
    pub module_source: bool,
    /// Leave `read_file` out of the agents' tools: every source is read through the code graph,
    /// and files outside it only through `grep`.
    #[arg(long, default_value_t = false)]
    pub no_read_file: bool,
    #[command(flatten)]
    pub matrix: MatrixArgs,
    #[command(flatten)]
    pub verify: VerifyArgs,
    #[command(flatten)]
    pub forge: ForgeArgs,
    #[command(flatten)]
    pub merge: MergeArgs,
    #[command(flatten)]
    pub llm: OpenAISetup,
}

#[derive(Args, Debug)]
pub struct InspectArgs {
    #[command(flatten)]
    pub project: ProjectArgs,
    /// Print the per-dimension inventories.
    #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
    pub inventory: bool,
    /// Print every call edge of one function (`Contract.function`) with its resolution.
    #[arg(long)]
    pub calls_of: Option<String>,
    /// Print the functions the matrix stages list for an anchor at one contract (`Contract`),
    /// function (`Contract.function`) or state variable (`Contract.variable`), optionally
    /// prefixed by its file (`src/Vault.sol:Vault.totalAssets`) when the name is not unique.
    #[arg(long)]
    pub functions_of: Option<String>,
    /// A local `forge` binary; with it the index comes from the compiler's ASTs instead of
    /// tree-sitter (unset: `forge` on PATH when present).
    #[arg(long = "forge-bin", env = "EVOLVE_FORGE_BIN")]
    pub forge_bin: Option<PathBuf>,
    /// Where the ASTs are written and reused; defaults to a directory under the system temp
    /// directory named after the project root.
    #[arg(long = "build-info-dir")]
    pub build_info_dir: Option<PathBuf>,
}

#[derive(Args, Debug)]
pub struct ReportArgs {
    #[arg(long, short = 'o')]
    pub out: PathBuf,
    #[arg(long, default_value_t = false)]
    pub no_merge: bool,
    #[command(flatten)]
    pub merge: MergeArgs,
    #[command(flatten)]
    pub llm: OpenAISetup,
}
