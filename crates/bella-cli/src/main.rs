//! The `bella` binary: one program for the controller, the worker, the
//! developer and agent commands, and the MCP bridge. It wires the library
//! crates together and adds no scheduling, protection or protocol logic of
//! its own. Commands and exit codes are documented in `docs/cli.md`.

mod admin;
mod cli;
mod controller;
mod ctx;
mod error;
mod jobs;
mod logging;
mod mcp;
mod output;
mod service;
mod work;
mod worker;

use clap::Parser;
use cli::{Cli, Command, ControllerCmd, WorkerCmd};
use ctx::Ctx;
use error::{CliResult, Exit};
use output::Out;
use std::process::ExitCode;

fn main() -> ExitCode {
    let cli = Cli::parse();
    let daemon = matches!(
        cli.command,
        Command::Controller(ControllerCmd::Run(_)) | Command::Worker(WorkerCmd::Run(_))
    );
    logging::init(cli.verbose, if daemon { "info" } else { "warn" });

    let ctx = Ctx {
        out: Out { json: cli.json },
        credentials: cli.credentials.clone(),
        verbose: cli.verbose,
    };
    match dispatch(&ctx, &cli.command) {
        Ok(exit) => ExitCode::from(exit.code()),
        Err(error) => {
            ctx.out.error(&error);
            ExitCode::from(error.exit.code())
        }
    }
}

fn dispatch(ctx: &Ctx, command: &Command) -> CliResult<Exit> {
    match command {
        Command::Controller(command) => match command {
            ControllerCmd::Run(args) => controller::run(ctx, args),
            ControllerCmd::Pair(args) => controller::pair(ctx, args),
            ControllerCmd::Service(command) => service::controller(ctx, command),
        },
        Command::Worker(command) => match command {
            WorkerCmd::Enroll(args) => worker::enroll(ctx, args),
            WorkerCmd::Run(args) => worker::run(ctx, args),
            WorkerCmd::Pause(args) => worker::local_control(ctx, args, |h| h.set_paused(true)),
            WorkerCmd::Resume(args) => worker::local_control(ctx, args, |h| h.set_paused(false)),
            WorkerCmd::Drain(args) => worker::local_control(ctx, args, |h| h.set_draining(true)),
            WorkerCmd::Undrain(args) => worker::local_control(ctx, args, |h| h.set_draining(false)),
            WorkerCmd::Boost(args) => worker::boost(ctx, args),
            WorkerCmd::Status(args) => worker::status(ctx, args),
            WorkerCmd::SelfTest(args) => worker::self_test(ctx, args),
            WorkerCmd::Service(command) => service::worker(ctx, command),
        },
        Command::Login(args) => admin::login(ctx, args),
        Command::Token(command) => admin::token(ctx, command),
        Command::Submit(args) => jobs::submit(ctx, args),
        Command::Plan(args) => jobs::plan(ctx, args),
        Command::Jobs(args) => jobs::jobs(ctx, args),
        Command::Job(args) => jobs::job(ctx, args),
        Command::Wait(args) => jobs::wait(ctx, args),
        Command::Logs(args) => jobs::logs(ctx, args),
        Command::Cancel(args) => jobs::cancel(ctx, args),
        Command::Artifacts(args) => jobs::artifacts(ctx, args),
        Command::Explain(args) => jobs::explain(ctx, args),
        Command::Env(command) => jobs::env(ctx, command),
        Command::Fleet => admin::fleet(ctx),
        Command::Queue => admin::queue(ctx),
        Command::Machine(command) => admin::machine(ctx, command),
        Command::Project(command) => admin::project(ctx, command),
        Command::Mcp(args) => mcp::run(ctx, args),
        Command::Version => admin::version(ctx),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn the_command_line_definition_is_consistent() {
        Cli::command().debug_assert();
    }
}
