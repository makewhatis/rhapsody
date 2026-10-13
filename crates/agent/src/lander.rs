//! Read-only post-deploy acceptance posture (STUDIO-1252); no Go counterpart.

pub const LANDER_TOOLS: &str = "Read,Glob,Grep,WebFetch,WebSearch";

pub fn lander_config(base: &crate::claude::Config) -> crate::claude::Config {
    let mut cfg = base.clone();
    cfg.permission_mode = "default".into();
    cfg.tools = Some(LANDER_TOOLS.into());
    cfg.allowed_tools = LANDER_TOOLS.into();
    cfg.disallowed_tools = "Bash,Write,Edit,NotebookEdit,Task,Agent,Skill".into();
    cfg.mcp_config = r#"{"mcpServers":{}}"#.into();
    cfg.inject_mcp = false;
    cfg.add_dirs.clear();
    cfg.setting_sources.clear();
    // Empty setting sources must be emitted, rather than inherited by the CLI.
    cfg.extra_args = vec![
        "--setting-sources".into(),
        String::new(),
        "--disable-slash-commands".into(),
    ];
    cfg.ultracode = false;
    cfg.turn_timeout = cfg.turn_timeout.min(std::time::Duration::from_secs(1800));
    cfg
}

#[cfg(test)]
mod tests {
    #[test]
    fn land_run_cannot_push_or_open_pr() {
        let cfg = super::lander_config(&crate::claude::Config {
            permission_mode: "bypassPermissions".into(),
            tools: Some("Bash".into()),
            allowed_tools: "*".into(),
            extra_args: vec!["--tools".into(), "Bash".into()],
            inject_mcp: true,
            mcp_config: "operator.json".into(),
            ..Default::default()
        });
        let args = crate::claude::build_args(&cfg, "");
        assert!(
            args.windows(2)
                .any(|w| w == ["--tools", "Read,Glob,Grep,WebFetch,WebSearch"])
        );
        assert!(
            args.windows(2)
                .any(|w| w == ["--permission-mode", "default"])
        );
        assert!(
            args.windows(2)
                .any(|w| w == ["--mcp-config", r#"{"mcpServers":{}}"#])
        );
        assert!(
            !args
                .iter()
                .any(|arg| arg == "bypassPermissions" || arg == "operator.json")
        );
        assert!(!cfg.inject_mcp);
        assert!(cfg.add_dirs.is_empty());
    }
}
