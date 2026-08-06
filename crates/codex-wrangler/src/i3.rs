use std::process::Command;

use anyhow::{Context as _, Result, bail};
use serde::Deserialize;
use x11rb::protocol::xproto::Window;

#[derive(Deserialize)]
struct Node {
    window: Option<Window>,
    #[serde(default)]
    floating: Floating,
    #[serde(default)]
    nodes: Vec<Self>,
    #[serde(default)]
    floating_nodes: Vec<Self>,
}

#[derive(Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Floating {
    AutoOn,
    UserOn,
    #[default]
    AutoOff,
    UserOff,
}

impl Node {
    fn floating(&self, quarry: Window, inherited: bool) -> Option<bool> {
        let floating = inherited || matches!(self.floating, Floating::AutoOn | Floating::UserOn);
        if self.window == Some(quarry) {
            return Some(floating);
        }
        self.nodes
            .iter()
            .find_map(|node| node.floating(quarry, floating))
            .or_else(|| {
                self.floating_nodes
                    .iter()
                    .find_map(|node| node.floating(quarry, true))
            })
    }
}

pub fn window_floating(window: Window) -> Result<Option<bool>> {
    let output = Command::new("i3-msg")
        .args(["-r", "-t", "get_tree"])
        .output()
        .context("query i3 layout tree")?;
    if !output.status.success() {
        bail!(
            "i3 layout query failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let tree: Node = serde_json::from_slice(&output.stdout).context("decode i3 layout tree")?;
    Ok(tree.floating(window, false))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn floating_branch_dominates_a_stale_child_flag() {
        let tree: Node = serde_json::from_str(
            r#"{
              "window": null,
              "nodes": [{"window": 7, "floating": "user_off"}],
              "floating_nodes": [{
                "window": null,
                "floating": "auto_on",
                "nodes": [{"window": 9, "floating": "user_off"}]
              }]
            }"#,
        )
        .expect("tree");
        assert_eq!(tree.floating(7, false), Some(false));
        assert_eq!(tree.floating(9, false), Some(true));
        assert_eq!(tree.floating(11, false), None);
    }
}
