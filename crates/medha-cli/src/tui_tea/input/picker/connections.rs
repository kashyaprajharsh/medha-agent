use super::*;

pub(super) fn activate(model: &mut Model) -> bool {
    let Some(picker) = model.picker.as_mut() else {
        return false;
    };
    if let PickerKind::McpTools { id, tools } = &picker.kind {
        let (id, sel) = (id.clone(), picker.selected);
        match tools.get(sel).cloned() {
            Some((name, on)) => mcp_set_tool(model, &id, &name, !on),
            None => open_mcp_picker(model),
        }
        return true;
    }
    if let PickerKind::McpAuth { id, url } = &picker.kind {
        let (id, url, sel) = (id.clone(), url.clone(), picker.selected);
        model.picker = None;
        mcp_choose_auth(model, &id, &url, sel);
        return true;
    }
    if let PickerKind::Mcp(rows) = &picker.kind {
        let sel = picker.selected;
        if sel == 0 {
            model.picker = None;
            prefill_command(
                model,
                "/mcp add ",
                "paste a server URL — e.g.  https://mcp.linear.app/mcp   (sign-in is automatic)   ·   or:  <id> -- npx -y @modelcontextprotocol/server-github",
            );
            return true;
        }
        let id = rows.get(sel - 1).map(|row| row.id.clone());
        model.picker = None;
        if let Some(id) = id {
            start_mcp_server(model, &id);
        }
        return true;
    }
    if let PickerKind::Connectors(picks) = &picker.kind {
        let id = picks.get(picker.selected).map(|pick| pick.id.clone());
        model.picker = None;
        if let Some(id) = id {
            connect_connector(model, &id);
        }
        return true;
    }
    if let PickerKind::McpCatalog(picks) = &picker.kind {
        let choice = picks.get(picker.selected).cloned();
        model.picker = None;
        if let Some(pick) = choice {
            model.input = pick.line;
            model.cursor = pick.cursor.min(model.input.len());
            model.push_notice(
                "fill in what's missing, then Enter to add it — registry listings aren't reviewed by Medha, so check the source first",
            );
        }
        return true;
    }
    false
}
