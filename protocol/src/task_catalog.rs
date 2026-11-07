//! Built-in template metadata shared by the offline profile compiler and
//! bootstrap's task CLI/daemon. Bootstrap's schema coverage tests check this
//! catalog against the actual instantiation path.

use serde::Serialize;

#[derive(Serialize, Clone, Copy)]
pub struct ParamSchema {
    pub name: &'static str,
    pub kind: &'static str,
    pub required: bool,
}

/// `run_once` is a presentation hint for the TUI, not an access restriction.
#[derive(Serialize)]
pub struct TemplateInfo {
    pub template: &'static str,
    pub params: &'static [ParamSchema],
    pub run_once: bool,
}

pub fn templates() -> Vec<TemplateInfo> {
    vec![
        TemplateInfo {
            template: "clone-git-repo",
            params: &[
                ParamSchema {
                    name: "repo",
                    kind: "string",
                    required: true,
                },
                ParamSchema {
                    name: "dest",
                    kind: "path",
                    required: true,
                },
            ],
            run_once: false,
        },
        TemplateInfo {
            template: "checkout-pr-worktree",
            params: &[
                ParamSchema {
                    name: "base",
                    kind: "path",
                    required: true,
                },
                ParamSchema {
                    name: "pr",
                    kind: "number",
                    required: true,
                },
                ParamSchema {
                    name: "dest",
                    kind: "path",
                    required: true,
                },
            ],
            run_once: false,
        },
        TemplateInfo {
            template: "install-starship",
            params: &[],
            run_once: true,
        },
        TemplateInfo {
            template: "install-toolchain-go",
            params: &[],
            run_once: true,
        },
        TemplateInfo {
            template: "install-toolchain-rust",
            params: &[],
            run_once: true,
        },
        TemplateInfo {
            template: "install-code-server",
            params: &[],
            run_once: false,
        },
        TemplateInfo {
            template: "wait-ssh-agent",
            params: &[],
            run_once: true,
        },
    ]
}
