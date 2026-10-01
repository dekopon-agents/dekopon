use serde_json::Value;

use super::{Builtin, BuiltinContext, CommandFailure, CommandResult};
use crate::{ExitCode, JOBS_OFF, JobId, JobState, JobWait};

pub(super) struct Jobs;
pub(super) struct Wait;
pub(super) struct Kill;

fn id(word: &str) -> Result<JobId, CommandFailure> {
    let digits = word.strip_prefix('%').unwrap_or(word);
    let number = digits
        .parse::<u64>()
        .map_err(|_invalid| CommandFailure::usage("expected a job id: %N or N"))?;
    if number == 0 {
        return Err(CommandFailure::usage("expected a job id: %N or N"));
    }
    Ok(JobId::new(number))
}

fn control<'a>(
    context: &'a BuiltinContext<'_>,
) -> Result<&'a dyn crate::JobControl, CommandFailure> {
    context
        .invoker
        .job_control()
        .ok_or_else(|| CommandFailure::failed(JOBS_OFF))
}

impl Builtin for Jobs {
    fn name(&self) -> &'static str {
        "jobs"
    }
    fn help(&self) -> &'static str {
        "List this person's jobs; -p prints ids only, -l keeps the normal listing."
    }
    fn run(
        &self,
        context: &mut BuiltinContext<'_>,
        arguments: &[String],
        _input: Option<Value>,
    ) -> Result<CommandResult, CommandFailure> {
        let ids_only = match arguments {
            [] => false,
            [one] if one == "-l" => false,
            [one] if one == "-p" => true,
            _ => return Err(CommandFailure::usage("usage: jobs [-l | -p]")),
        };
        let lines = control(context)?
            .list()
            .into_iter()
            .map(|row| {
                if ids_only {
                    return row.id.to_string();
                }
                let end = row
                    .text
                    .char_indices()
                    .map(|(index, _)| index)
                    .take_while(|index| *index <= 200)
                    .last()
                    .unwrap_or(0);
                let text = if row.text.len() <= 200 {
                    &row.text
                } else {
                    &row.text[..end]
                };
                match row.state {
                    JobState::Running { elapsed } => {
                        format!("[{}] running {}s {text}", row.id, elapsed.as_secs())
                    }
                    JobState::Finished {
                        outcome,
                        exit,
                        after,
                    } => format!(
                        "[{}] {outcome} exit {} after {}s {text}",
                        row.id,
                        exit.get(),
                        after.as_secs()
                    ),
                }
            })
            .collect();
        Ok(CommandResult::lines(lines))
    }
}

impl Builtin for Wait {
    fn name(&self) -> &'static str {
        "wait"
    }
    fn help(&self) -> &'static str {
        "Wait for this script's jobs, or for named job ids."
    }
    fn run(
        &self,
        context: &mut BuiltinContext<'_>,
        arguments: &[String],
        _input: Option<Value>,
    ) -> Result<CommandResult, CommandFailure> {
        let control = control(context)?;
        let ids = if arguments.is_empty() {
            context.started_jobs.to_vec()
        } else {
            arguments
                .iter()
                .map(|word| id(word))
                .collect::<Result<Vec<_>, _>>()?
        };
        let mut status = ExitCode::SUCCESS;
        let mut refusal = None;
        let answers = control.wait(&ids, &|| {
            !context.invoker.cancelled() && context.budget.check_deadline().is_ok()
        });
        for answer in answers {
            match answer {
                Ok(JobWait::Exited(exit)) => status = exit,
                Ok(JobWait::Interrupted) => {
                    context.budget.charge_step_with(context.invoker)?;
                    return Ok(CommandResult::status(status));
                }
                Err(error) => refusal = Some(error),
            }
        }
        if let Some(error) = refusal {
            return Err(CommandFailure::failed(error.to_string()));
        }
        Ok(CommandResult::status(status))
    }
}

impl Builtin for Kill {
    fn name(&self) -> &'static str {
        "kill"
    }
    fn help(&self) -> &'static str {
        "Cancel a job by id; signal options are accepted but all cancel."
    }
    fn run(
        &self,
        context: &mut BuiltinContext<'_>,
        arguments: &[String],
        _input: Option<Value>,
    ) -> Result<CommandResult, CommandFailure> {
        let control = control(context)?;
        let mut words = arguments.iter();
        let first = words
            .next()
            .ok_or_else(|| CommandFailure::usage("usage: kill [-s SIG | -SIG] job ..."))?;
        let first_id = if first == "-s" {
            words
                .next()
                .ok_or_else(|| CommandFailure::usage("usage: kill -s SIG job ..."))?;
            None
        } else if first.starts_with('-') {
            None
        } else {
            Some(first)
        };
        let ids = first_id
            .into_iter()
            .chain(words)
            .map(|word| id(word))
            .collect::<Result<Vec<_>, _>>()?;
        if ids.is_empty() {
            return Err(CommandFailure::usage("usage: kill [-s SIG | -SIG] job ..."));
        }
        let mut refusal = None;
        for id in ids {
            if let Err(error) = control.kill(id) {
                refusal = Some(error);
            }
        }
        if let Some(error) = refusal {
            return Err(CommandFailure::failed(error.to_string()));
        }
        Ok(CommandResult::status(ExitCode::SUCCESS))
    }
}
