//! `/hooks`: add a project hook by answering two questions, without learning a
//! file format. The result is an ordinary script in `.medha/hooks/<event>/`.

use super::{Screen, review_hooks};
pub(super) use crate::hook_files::{EVENTS, TOOLS, add};
use crate::tui_tea::{Model, Picker, PickerKind};

pub(super) fn event_labels() -> Vec<String> {
    EVENTS
        .iter()
        .map(|(event, description, _)| format!("{event} — {description}"))
        .chain(std::iter::once(
            "📋 See installed hooks and plugins".to_string(),
        ))
        .collect()
}

pub(super) fn tool_labels() -> Vec<String> {
    TOOLS
        .iter()
        .map(|(_, label)| (*label).to_string())
        .collect()
}

pub(super) fn open(model: &mut Model) {
    model.picker = Some(Picker::new(PickerKind::Plugins(Screen::HookEvent)));
}

pub(super) fn choose_event(model: &mut Model, selected: usize) {
    match EVENTS.get(selected) {
        Some((event, _, true)) => {
            model.picker = Some(Picker::new(PickerKind::Plugins(Screen::HookTools(
                (*event).to_string(),
            ))));
        }
        Some((event, _, false)) => ask_for_command(model, event, "*"),
        None => super::open(model),
    }
}

pub(super) fn choose_tools(model: &mut Model, event: &str, selected: usize) {
    let matcher = TOOLS.get(selected).map_or("*", |(matcher, _)| matcher);
    ask_for_command(model, event, matcher);
}

fn ask_for_command(model: &mut Model, event: &str, matcher: &str) {
    model.picker = None;
    model.input = format!("/hooks add {event} {matcher} ");
    model.cursor = model.input.len();
    model.push_notice(
        "type a shell command, or paste the path to a script to copy in, then Enter — \
         exit 0 continues, exit 2 blocks and shows stderr",
    );
}

/// `/hooks`, or `/hooks add <event> <matcher> <command | script path>`.
pub(crate) fn command(model: &mut Model, args: &str) {
    let mut parts = args.splitn(4, ' ');
    match (parts.next(), parts.next(), parts.next(), parts.next()) {
        (Some("add"), Some(event), Some(matcher), Some(body)) if !body.trim().is_empty() => {
            let workspace = model.restore.root().to_path_buf();
            match add(&workspace, event, matcher, body.trim()) {
                Ok(path) => {
                    model.push_notice(format!("added hook {}", path.display()));
                    review_hooks(model);
                }
                Err(error) => model.push_notice(format!("hooks: {error}")),
            }
        }
        (None | Some(""), ..) => open(model),
        _ => model.push_notice("usage: /hooks · /hooks add <event> <tools|*> <command or script>"),
    }
}
