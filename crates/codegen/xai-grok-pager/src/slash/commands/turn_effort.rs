//! `/low`, `/medium`, `/high`, `/xhigh` — send this prompt at a turn-scoped
//! reasoning effort without changing the session `/effort`.

use agent_client_protocol as acp;
use xai_grok_shell::sampling::types::ReasoningEffort;

use crate::slash::command::{AppCtx, CommandExecCtx, CommandResult, SlashCommand};

/// One-shot turn effort: `/high fix this` samples this turn at `high`.
pub struct TurnEffortCommand {
    pub level: ReasoningEffort,
}

impl TurnEffortCommand {
    pub const fn new(level: ReasoningEffort) -> Self {
        Self { level }
    }

    fn usage_string(&self) -> String {
        format!("/{} <prompt>", self.level.as_str())
    }
}

impl SlashCommand for TurnEffortCommand {
    fn name(&self) -> &str {
        self.level.as_str()
    }

    fn description(&self) -> &str {
        match self.level {
            ReasoningEffort::Low => "Send this prompt at low reasoning effort",
            ReasoningEffort::Medium => "Send this prompt at medium reasoning effort",
            ReasoningEffort::High => "Send this prompt at high reasoning effort",
            ReasoningEffort::Xhigh => "Send this prompt at extra-high reasoning effort",
            ReasoningEffort::None | ReasoningEffort::Minimal | ReasoningEffort::Max => {
                "Send this prompt at a one-shot reasoning effort"
            }
        }
    }

    fn session_scoped(&self) -> bool {
        true
    }

    fn usage(&self) -> &str {
        match self.level {
            ReasoningEffort::Low => "/low <prompt>",
            ReasoningEffort::Medium => "/medium <prompt>",
            ReasoningEffort::High => "/high <prompt>",
            ReasoningEffort::Xhigh => "/xhigh <prompt>",
            ReasoningEffort::None | ReasoningEffort::Minimal | ReasoningEffort::Max => {
                "/effort <prompt>"
            }
        }
    }

    fn takes_args(&self) -> bool {
        true
    }

    fn args_required(&self) -> bool {
        true
    }

    fn arg_placeholder(&self) -> Option<&str> {
        Some("<prompt>")
    }

    fn visible(&self, ctx: &AppCtx) -> bool {
        ctx.models
            .reasoning_effort_options()
            .iter()
            .any(|opt| opt.value == self.level)
    }

    fn run(&self, ctx: &mut CommandExecCtx, args: &str) -> CommandResult {
        let prompt = args.trim();
        if prompt.is_empty() {
            return CommandResult::Error(format!("Usage: {}", self.usage_string()));
        }

        let Some(model_id) = ctx.models.current.clone() else {
            return CommandResult::Error("No active model".into());
        };

        match ctx
            .models
            .resolve_effort_for_model(&model_id, self.level.as_str())
        {
            Ok(effort) => CommandResult::InjectSkill {
                display_text: format!("/{} {prompt}", self.level.as_str()),
                prompt_blocks: vec![acp::ContentBlock::Text(acp::TextContent::new(
                    prompt.to_string(),
                ))],
                display_as_skill: true,
                scheduled_task_preview: None,
                reasoning_effort: Some(effort),
            },
            Err(err) => CommandResult::Error(err.message()),
        }
    }
}

/// Builtins offered as turn-scoped effort commands, strongest first.
pub fn builtins() -> [TurnEffortCommand; 4] {
    [
        TurnEffortCommand::new(ReasoningEffort::Xhigh),
        TurnEffortCommand::new(ReasoningEffort::High),
        TurnEffortCommand::new(ReasoningEffort::Medium),
        TurnEffortCommand::new(ReasoningEffort::Low),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::model_state::ModelState;
    use crate::slash::command::CommandResult;
    use std::sync::Arc;

    fn model_with_reasoning(id: &str, name: &str) -> (acp::ModelId, acp::ModelInfo) {
        let id = acp::ModelId::new(Arc::from(id));
        let mut meta = serde_json::Map::new();
        meta.insert(
            "supportsReasoningEffort".into(),
            serde_json::Value::Bool(true),
        );
        let info = acp::ModelInfo::new(id.clone(), name.to_string())
            .meta(serde_json::Value::Object(meta).as_object().cloned());
        (id, info)
    }

    fn model_without_reasoning(id: &str, name: &str) -> (acp::ModelId, acp::ModelInfo) {
        let id = acp::ModelId::new(Arc::from(id));
        let info = acp::ModelInfo::new(id.clone(), name.to_string());
        (id, info)
    }

    fn grok_45_high_only(id: &str, name: &str) -> (acp::ModelId, acp::ModelInfo) {
        let id = acp::ModelId::new(Arc::from(id));
        let mut meta = serde_json::Map::new();
        meta.insert(
            "supportsReasoningEffort".into(),
            serde_json::Value::Bool(true),
        );
        meta.insert(
            "reasoningEfforts".into(),
            serde_json::json!([
                { "value": "high", "label": "High", "default": true },
                { "value": "medium", "label": "Medium" },
                { "value": "low", "label": "Low" },
            ]),
        );
        let info = acp::ModelInfo::new(id.clone(), name.to_string())
            .meta(serde_json::Value::Object(meta).as_object().cloned());
        (id, info)
    }

    fn dummy_exec_ctx(models: &ModelState) -> crate::slash::command::CommandExecCtx<'_> {
        super::super::tests::make_ctx(models)
    }

    fn app_ctx<'a>(models: &'a ModelState) -> AppCtx<'a> {
        AppCtx {
            models,
            cwd: std::path::Path::new("."),
            has_session_announcements: false,
            billing_surface_visible: true,
            usage_command_visible: true,
            workflows_available: true,
            saved_workflows: &[],
            workflow_runs: &[],
            screen_mode: crate::app::ScreenMode::Fullscreen,
            current_title: None,
        }
    }

    #[test]
    fn empty_args_errors_with_usage() {
        let mut state = ModelState::default();
        let (id, info) = model_with_reasoning("grok-4.6", "Grok 4.6");
        state.available.insert(id.clone(), info);
        state.current = Some(id);
        let mut ctx = dummy_exec_ctx(&state);
        let result = TurnEffortCommand::new(ReasoningEffort::High).run(&mut ctx, "");
        match result {
            CommandResult::Error(msg) => {
                assert!(msg.contains("Usage: /high <prompt>"), "msg={msg}")
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn no_model_errors() {
        let state = ModelState::default();
        let mut ctx = dummy_exec_ctx(&state);
        let result = TurnEffortCommand::new(ReasoningEffort::High).run(&mut ctx, "fix this");
        match result {
            CommandResult::Error(msg) => assert!(msg.contains("No active model"), "msg={msg}"),
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn unsupported_model_errors() {
        let mut state = ModelState::default();
        let (id, info) = model_without_reasoning("plain", "Plain");
        state.available.insert(id.clone(), info);
        state.current = Some(id);
        let mut ctx = dummy_exec_ctx(&state);
        let result = TurnEffortCommand::new(ReasoningEffort::High).run(&mut ctx, "fix this");
        match result {
            CommandResult::Error(msg) => {
                assert!(
                    msg.contains("does not support reasoning effort"),
                    "msg={msg}"
                )
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn xhigh_rejected_when_model_menu_omits_it() {
        let mut state = ModelState::default();
        let (id, info) = grok_45_high_only("grok-4.5", "Grok 4.5");
        state.available.insert(id.clone(), info);
        state.current = Some(id);
        let mut ctx = dummy_exec_ctx(&state);
        let result = TurnEffortCommand::new(ReasoningEffort::Xhigh).run(&mut ctx, "dig in");
        match result {
            CommandResult::Error(msg) => {
                assert!(msg.contains("unknown effort level 'xhigh'"), "msg={msg}");
                assert!(msg.contains("high"), "msg={msg}");
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn high_enqueues_prompt_without_slash_on_the_wire() {
        let mut state = ModelState::default();
        let (id, info) = model_with_reasoning("grok-4.6", "Grok 4.6");
        state.available.insert(id.clone(), info);
        state.current = Some(id);
        let mut ctx = dummy_exec_ctx(&state);
        let result = TurnEffortCommand::new(ReasoningEffort::High).run(&mut ctx, "fix this");
        match result {
            CommandResult::InjectSkill {
                display_text,
                prompt_blocks,
                display_as_skill,
                reasoning_effort,
                ..
            } => {
                assert_eq!(display_text, "/high fix this");
                assert!(display_as_skill);
                assert_eq!(reasoning_effort, Some(ReasoningEffort::High));
                let acp::ContentBlock::Text(text) = &prompt_blocks[0] else {
                    panic!("expected text block");
                };
                assert_eq!(text.text, "fix this");
            }
            other => panic!("expected InjectSkill, got {other:?}"),
        }
    }

    #[test]
    fn visible_only_when_current_model_offers_the_level() {
        let mut state = ModelState::default();
        let (id, info) = grok_45_high_only("grok-4.5", "Grok 4.5");
        state.available.insert(id.clone(), info);
        state.current = Some(id);
        let ctx = app_ctx(&state);
        assert!(TurnEffortCommand::new(ReasoningEffort::High).visible(&ctx));
        assert!(TurnEffortCommand::new(ReasoningEffort::Low).visible(&ctx));
        assert!(!TurnEffortCommand::new(ReasoningEffort::Xhigh).visible(&ctx));
    }

    #[test]
    fn builtins_cover_low_through_xhigh() {
        let cmds = builtins();
        let names: Vec<&str> = cmds.iter().map(|c| c.name()).collect();
        assert_eq!(names, ["xhigh", "high", "medium", "low"]);
    }
}
