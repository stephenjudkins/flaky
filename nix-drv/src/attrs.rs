//! Generation of `.attrs.sh` / `.attrs.json` for derivations using
//! `__structuredAttrs`, mirroring what nix writes into the build sandbox.

use serde_json::Value;

/// Single-quote a string for bash, using the '\'' escape.
pub fn shell_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        if c == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
}

fn scalar(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(true) => Some("1".to_string()),
        Value::Bool(false) | Value::Null => None,
        _ => None,
    }
}

fn element_str(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Bool(true) => "1".to_string(),
        Value::Bool(false) | Value::Null => String::new(),
        Value::Object(m) if m.is_empty() => String::new(),
        other => other.to_string(),
    }
}

fn space_joined(v: &Value) -> Option<String> {
    match v {
        Value::Array(items) => Some(items.iter().map(element_str).collect::<Vec<_>>().join(" ")),
        other => scalar(other),
    }
}

/// Flattened process environment for a structured-attrs derivation: the
/// members of the `env` attrset (lists space-joined).
pub fn structured_env(attrs: &Value) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let Some(obj) = attrs.as_object() else {
        return out;
    };
    if let Some(env) = obj.get("env").and_then(|e| e.as_object()) {
        for (k, v) in env {
            if let Some(s) = space_joined(v) {
                out.push((k.clone(), s));
            }
        }
    }
    out
}

/// Render `.attrs.sh` for the parsed `__json` value. Output paths are passed
/// explicitly (`__json.outputs` holds only output names); they become the
/// `outputs` associative array plus one export per output. Top-level attrs
/// become exports / `declare -a` / `declare -A`, and the `env` attrset is
/// flattened into exports.
pub fn generate_attrs_sh(attrs: &Value, outputs: &[(String, String)]) -> String {
    let mut out = String::new();
    let entries: Vec<String> = outputs
        .iter()
        .map(|(name, path)| format!("[{}]={}", shell_quote(name), shell_quote(path)))
        .collect();
    out.push_str(&format!("declare -A outputs=( {} )\n", entries.join(" ")));
    for (name, path) in outputs {
        out.push_str(&format!("export {}={}\n", name, shell_quote(path)));
    }
    let Some(obj) = attrs.as_object() else {
        return out;
    };
    for (k, v) in obj {
        if k == "outputs" {
            continue;
        }
        if k == "env" {
            if let Some(env) = v.as_object() {
                for (ek, ev) in env {
                    if let Some(s) = space_joined(ev) {
                        out.push_str(&format!("export {ek}={}\n", shell_quote(&s)));
                    }
                }
            }
            continue;
        }
        match v {
            Value::Array(items) => {
                let elems: Vec<String> = items
                    .iter()
                    .map(|it| shell_quote(&element_str(it)))
                    .collect();
                out.push_str(&format!("declare -a {k}=( {} )\n", elems.join(" ")));
            }
            Value::Object(m) => {
                let entries: Vec<String> = m
                    .iter()
                    .map(|(ik, iv)| {
                        format!("[{}]={}", shell_quote(ik), shell_quote(&element_str(iv)))
                    })
                    .collect();
                out.push_str(&format!("declare -A {k}=( {} )\n", entries.join(" ")));
            }
            _ => {
                if let Some(s) = scalar(v) {
                    out.push_str(&format!("export {k}={}\n", shell_quote(&s)));
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn hello_like() -> Value {
        json!({
            "name": "hello-2.12.3",
            "version": "2.12.3",
            "strictDeps": false,
            "doCheck": true,
            "patches": [],
            "outputs": ["out"],
            "nativeBuildInputs": ["/nix/store/p1-pkg", "/nix/store/p2-pkg"],
            "outputChecks": {"out": {}},
            "builder": "/nix/store/dddd-bash/bin/bash",
            "stdenv": "/nix/store/std-stdenv",
            "src": "/nix/store/wj7-hello.tar.gz",
            "env": {
                "NIX_HARDENING_ENABLE": "fortify",
                "emptyBool": false
            },
            "system": "aarch64-linux"
        })
    }

    const OUTS: &[(&str, &str)] = &[("out", "/nix/store/eee-hello")];

    #[test]
    fn quotes() {
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
    }

    #[test]
    fn generates_attrs_sh() {
        let sh = generate_attrs_sh(
            &hello_like(),
            &OUTS
                .iter()
                .map(|(n, p)| (n.to_string(), p.to_string()))
                .collect::<Vec<_>>(),
        );
        assert!(sh.contains("declare -A outputs=( ['out']='/nix/store/eee-hello' )"));
        assert!(sh.contains("export out='/nix/store/eee-hello'"));
        assert!(sh.contains("export name='hello-2.12.3'"));
        assert!(sh.contains("export doCheck='1'"));
        assert!(!sh.contains("strictDeps"), "false booleans must be unset");
        assert!(sh.contains("declare -a patches=(  )"));
        assert!(
            sh.contains("declare -a nativeBuildInputs=( '/nix/store/p1-pkg' '/nix/store/p2-pkg' )")
        );
        assert!(sh.contains("declare -A outputChecks=( ['out']='' )"));
        assert!(sh.contains("export stdenv='/nix/store/std-stdenv'"));
        assert!(sh.contains("export src='/nix/store/wj7-hello.tar.gz'"));
        assert!(sh.contains("export NIX_HARDENING_ENABLE='fortify'"));
        assert!(!sh.contains("emptyBool"));
        assert!(
            !sh.contains("declare -a outputs"),
            "outputs must stay an assoc array"
        );
    }

    #[test]
    fn flattens_env() {
        let env = structured_env(&hello_like());
        let m: std::collections::BTreeMap<_, _> = env.into_iter().collect();
        assert_eq!(m["NIX_HARDENING_ENABLE"], "fortify");
        assert!(!m.contains_key("emptyBool"));
    }
}
