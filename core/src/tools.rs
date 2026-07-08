//! Tool registry.
//!
//! Tools are declarative: name + description + typed parameters + a shell
//! command template. From the registry we auto-generate BOTH sides of the
//! contract:
//!   1. a system-prompt block describing the tools and required reply shape
//!   2. a JSON Schema passed as Ollama's `format`, so decoding is
//!      grammar-constrained and *every* model — tool-trained or not —
//!      physically cannot emit anything but valid tool-call JSON.

use serde::{Deserialize, Serialize};
use std::process::Command;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ParamType {
    String,
    Number,
    Boolean,
}

impl ParamType {
    pub fn json_name(self) -> &'static str {
        match self {
            ParamType::String => "string",
            ParamType::Number => "number",
            ParamType::Boolean => "boolean",
        }
    }
    pub const ALL: [ParamType; 3] = [ParamType::String, ParamType::Number, ParamType::Boolean];
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolParam {
    pub name: String,
    pub description: String,
    pub ptype: ParamType,
    pub required: bool,
}

/// How a tool is executed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ToolKind {
    /// Run `command` via `sh -c` with escaped `{param}` substitution.
    #[default]
    Shell,
    /// Handled by the app itself: the model's arguments describe a NEW tool,
    /// which is validated and added to the live registry (see `build_tool_from_args`).
    CreateTool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    pub params: Vec<ToolParam>,
    /// Shell command template. `{param_name}` placeholders are replaced with
    /// (single-quote-escaped) argument values, then run via `sh -c`.
    /// SECURITY: tools you add here run with your user's privileges.
    pub command: String,
    pub enabled: bool,
    /// Defaults to Shell so tool files saved by older versions still load.
    #[serde(default)]
    pub kind: ToolKind,
    /// True if this tool was authored by the model via `create_tool`.
    #[serde(default)]
    pub ai_created: bool,
}

impl ToolDef {
    pub fn execute(&self, args: &serde_json::Value) -> String {
        let mut cmd = self.command.clone();
        for p in &self.params {
            let raw = match &args[&p.name] {
                serde_json::Value::String(s) => s.clone(),
                serde_json::Value::Null => String::new(),
                other => other.to_string(),
            };
            // Safe single-quote shell escaping: ' -> '\''
            let escaped = format!("'{}'", raw.replace('\'', r"'\''"));
            cmd = cmd.replace(&format!("{{{}}}", p.name), &escaped);
        }
        match Command::new("sh").arg("-c").arg(&cmd).output() {
            Ok(out) => {
                let mut s = String::from_utf8_lossy(&out.stdout).to_string();
                let err = String::from_utf8_lossy(&out.stderr);
                if !err.trim().is_empty() {
                    s.push_str("\n[stderr] ");
                    s.push_str(err.trim());
                }
                if s.trim().is_empty() {
                    s = format!("(command exited with {}, no output)", out.status);
                }
                // Keep tool results bounded so they don't blow the context.
                const MAX: usize = 6000;
                if s.len() > MAX {
                    let mut cut = MAX;
                    while cut > 0 && !s.is_char_boundary(cut) {
                        cut -= 1;
                    }
                    s.truncate(cut);
                    s.push_str("\n…[truncated]");
                }
                s
            }
            Err(e) => format!("tool execution error: {e}"),
        }
    }
}

/// Built-in starter tools. Add/remove/edit freely from the UI.
pub fn default_tools() -> Vec<ToolDef> {
    vec![
        ToolDef {
            name: "current_time".into(),
            description: "Get the current local date and time.".into(),
            params: vec![],
            command: "date".into(),
            enabled: true,
            kind: ToolKind::Shell,
            ai_created: false,
        },
        ToolDef {
            name: "run_shell".into(),
            description: "Run a shell command on the local machine and return its output.".into(),
            params: vec![ToolParam {
                name: "command".into(),
                description: "The shell command to execute".into(),
                ptype: ParamType::String,
                required: true,
            }],
            // The value arrives quoted, so run it through `sh -c` again:
            command: "sh -c {command}".into(),
            enabled: false, // powerful — opt in from the UI
            kind: ToolKind::Shell,
            ai_created: false,
        },
        ToolDef {
            name: "read_file".into(),
            description: "Read a text file from disk (first 200 lines).".into(),
            params: vec![ToolParam {
                name: "path".into(),
                description: "Absolute path of the file".into(),
                ptype: ParamType::String,
                required: true,
            }],
            command: "head -n 200 {path}".into(),
            enabled: true,
            kind: ToolKind::Shell,
            ai_created: false,
        },
        create_tool_def(),
    ]
}

/// The reply envelope every model turn must fit. `action` discriminates
/// between calling a tool and finishing.
pub fn response_schema(tools: &[ToolDef]) -> serde_json::Value {
    let enabled: Vec<&ToolDef> = tools.iter().filter(|t| t.enabled).collect();
    let tool_names: Vec<&str> = enabled.iter().map(|t| t.name.as_str()).collect();

    // Union of all tool params so `arguments` stays a plain object schema.
    // (Per-tool validation happens app-side; the grammar guarantees shape.)
    let mut arg_props = serde_json::Map::new();
    for t in &enabled {
        for p in &t.params {
            arg_props.insert(
                p.name.clone(),
                serde_json::json!({ "type": p.ptype.json_name(), "description": p.description }),
            );
        }
    }

    serde_json::json!({
        "type": "object",
        "properties": {
            "thought": {
                "type": "string",
                "description": "Brief reasoning about what to do next"
            },
            "action": {
                "type": "string",
                "enum": ["tool_call", "final_answer"]
            },
            "tool": {
                "type": "string",
                "enum": tool_names,
                "description": "Name of the tool to call (when action=tool_call)"
            },
            "arguments": {
                "type": "object",
                "properties": arg_props,
                "description": "Arguments for the tool (when action=tool_call)"
            },
            "final_answer": {
                "type": "string",
                "description": "The answer for the user (when action=final_answer)"
            }
        },
        "required": ["thought", "action"]
    })
}

/// Auto-generated system prompt block appended after the user's own prompt.
pub fn system_prompt_block(tools: &[ToolDef]) -> String {
    let enabled: Vec<&ToolDef> = tools.iter().filter(|t| t.enabled).collect();
    if enabled.is_empty() {
        return String::new();
    }
    let mut s = String::from(
        "\n\n# Tools\n\
         You can call tools. On every turn respond with ONE JSON object and nothing else:\n\
         - To call a tool: {\"thought\": \"...\", \"action\": \"tool_call\", \"tool\": \"<name>\", \"arguments\": { ... }}\n\
         - To finish:      {\"thought\": \"...\", \"action\": \"final_answer\", \"final_answer\": \"...\"}\n\
         After a tool call you will receive a message with role \"tool\" containing the result. \
         Use as many tool calls as needed before giving the final answer.\n\n\
         Available tools:\n",
    );
    for t in enabled {
        s.push_str(&format!("- {}: {}\n", t.name, t.description));
        for p in &t.params {
            s.push_str(&format!(
                "    - {} ({}{}): {}\n",
                p.name,
                p.ptype.json_name(),
                if p.required { ", required" } else { "" },
                p.description
            ));
        }
    }
    s
}

// ---------------------------------------------------------------------------
// AI-authored tools: the `create_tool` meta-tool
// ---------------------------------------------------------------------------

/// The built-in meta-tool. Its description + param descriptions are the
/// model's entire documentation (they land in the system prompt verbatim),
/// so they spell out the exact format and the research-then-build workflow.
pub fn create_tool_def() -> ToolDef {
    ToolDef {
        name: "create_tool".into(),
        description: "Create a NEW tool and add it to this app's tool registry. \
            The new tool becomes available to you on your NEXT step, so you can call it \
            immediately to test it. Use this when the user asks you to build or add a tool. \
            A tool wraps one shell command template: at call time each {param_name} \
            placeholder is replaced with the shell-escaped argument value. \
            If you need information first (a program's flags via `--help`, an API's URL \
            shape, the format of a config file), gather it with run_shell or read_file \
            BEFORE creating the tool, and base the command on what you actually observed."
            .into(),
        params: vec![
            ToolParam {
                name: "name".into(),
                description: "snake_case tool name, unique in the registry".into(),
                ptype: ParamType::String,
                required: true,
            },
            ToolParam {
                name: "description".into(),
                description: "What the tool does and when to use it (written for an AI)".into(),
                ptype: ParamType::String,
                required: true,
            },
            ToolParam {
                name: "command".into(),
                description: "Shell command template with {param} placeholders, e.g. \
                    curl -s 'https://wttr.in/{city}?format=3'. Every placeholder must \
                    match a declared parameter."
                    .into(),
                ptype: ParamType::String,
                required: true,
            },
            ToolParam {
                name: "parameters".into(),
                description: "JSON array as a STRING describing the new tool's parameters, \
                    e.g. [{\"name\":\"city\",\"type\":\"string\",\"description\":\"city name\",\
                    \"required\":true}]. Types: string|number|boolean. Use [] for none."
                    .into(),
                ptype: ParamType::String,
                required: true,
            },
        ],
        command: String::new(),
        enabled: true,
        kind: ToolKind::CreateTool,
        ai_created: false,
    }
}

fn parse_ptype(s: &str) -> Option<ParamType> {
    match s.trim().to_ascii_lowercase().as_str() {
        "string" | "str" | "text" => Some(ParamType::String),
        "number" | "num" | "int" | "integer" | "float" | "double" => Some(ParamType::Number),
        "boolean" | "bool" => Some(ParamType::Boolean),
        _ => None,
    }
}

/// Extract `{placeholder}` names from a command template.
fn placeholders(cmd: &str) -> Vec<String> {
    let mut out = Vec::new();
    let bytes = cmd.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'{' {
            if let Some(end) = cmd[i + 1..].find('}') {
                let inner = &cmd[i + 1..i + 1 + end];
                if !inner.is_empty()
                    && inner.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                {
                    out.push(inner.to_string());
                }
                i += end + 2;
                continue;
            }
        }
        i += 1;
    }
    out
}

/// Validate the arguments of a `create_tool` call against the current
/// registry and build the new ToolDef. Errors are written so the model can
/// read them as a tool result and correct itself on the next step.
pub fn build_tool_from_args(
    args: &serde_json::Value,
    existing: &[ToolDef],
) -> Result<ToolDef, String> {
    let get = |k: &str| args[k].as_str().map(str::trim).unwrap_or("").to_string();

    let name = get("name").to_ascii_lowercase();
    if name.is_empty() {
        return Err("error: 'name' is required".into());
    }
    if !name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        || !name.chars().next().is_some_and(|c| c.is_ascii_lowercase())
    {
        return Err(format!(
            "error: name '{name}' must be snake_case (lowercase letters, digits, underscores, starting with a letter)"
        ));
    }
    if existing.iter().any(|t| t.name == name) {
        return Err(format!(
            "error: a tool named '{name}' already exists — choose a different name"
        ));
    }

    let command = get("command");
    if command.is_empty() {
        return Err("error: 'command' is required".into());
    }

    // `parameters` arrives as a JSON array in a string (the response grammar
    // flattens tool arguments to scalars). Be lenient: accept an actual array
    // too, and an empty string as "no parameters".
    let params_raw = &args["parameters"];
    let params_val: serde_json::Value = match params_raw {
        serde_json::Value::Array(_) => params_raw.clone(),
        serde_json::Value::String(s) if s.trim().is_empty() => serde_json::json!([]),
        serde_json::Value::String(s) => serde_json::from_str(s.trim()).map_err(|e| {
            format!("error: 'parameters' is not valid JSON ({e}). Expected e.g. [{{\"name\":\"x\",\"type\":\"string\",\"description\":\"...\",\"required\":true}}]")
        })?,
        serde_json::Value::Null => serde_json::json!([]),
        _ => return Err("error: 'parameters' must be a JSON array (as a string)".into()),
    };
    let Some(items) = params_val.as_array() else {
        return Err("error: 'parameters' must be a JSON ARRAY of parameter objects".into());
    };

    let mut params = Vec::new();
    for it in items {
        let pname = it["name"].as_str().unwrap_or("").trim().to_string();
        if pname.is_empty() {
            return Err("error: every parameter needs a non-empty 'name'".into());
        }
        let tstr = it["type"].as_str().or_else(|| it["ptype"].as_str()).unwrap_or("string");
        let ptype = parse_ptype(tstr)
            .ok_or_else(|| format!("error: parameter '{pname}' has unknown type '{tstr}' (use string|number|boolean)"))?;
        params.push(ToolParam {
            name: pname,
            description: it["description"].as_str().unwrap_or("").trim().to_string(),
            ptype,
            required: it["required"].as_bool().unwrap_or(true),
        });
    }

    // Every placeholder in the command must be a declared parameter —
    // otherwise it would be passed to the shell literally.
    for ph in placeholders(&command) {
        if !params.iter().any(|p| p.name == ph) {
            return Err(format!(
                "error: command references {{{ph}}} but no parameter '{ph}' is declared. \
                 Declare it in 'parameters' or remove the placeholder."
            ));
        }
    }

    Ok(ToolDef {
        name,
        description: get("description"),
        params,
        command,
        enabled: true,
        kind: ToolKind::Shell,
        ai_created: true,
    })
}

// ---------------------------------------------------------------------------
// Persistence — AI-created tools must survive restarts to be worth anything.
// ---------------------------------------------------------------------------

pub fn config_dir() -> std::path::PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::path::PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into()))
                .join(".config")
        })
        .join("llm-desk")
}

pub fn load_tools() -> Vec<ToolDef> {
    let path = config_dir().join("tools.json");
    let mut tools: Vec<ToolDef> = std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(default_tools);
    // Make sure the meta-tool exists even in registries saved by older versions.
    if !tools.iter().any(|t| t.kind == ToolKind::CreateTool) {
        tools.push(create_tool_def());
    }
    tools
}

pub fn save_tools(tools: &[ToolDef]) {
    let dir = config_dir();
    let _ = std::fs::create_dir_all(&dir);
    if let Ok(json) = serde_json::to_string_pretty(tools) {
        let _ = std::fs::write(dir.join("tools.json"), json);
    }
}
