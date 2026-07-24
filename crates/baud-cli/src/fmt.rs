// Copyright (c) 2026 Henrique Falconer. All rights reserved.
// SPDX-License-Identifier: Proprietary

use serde_json::Value;

/// Print a JSON value, either raw (--json) or as a human-readable table/summary.
pub fn print_value(v: &Value, json: bool) {
    print(v, json)
}

/// Print a JSON value, either raw (--json) or as a human-readable table/summary.
pub fn print(v: &Value, json: bool) {
    if json {
        println!("{}", serde_json::to_string_pretty(v).unwrap_or_else(|_| v.to_string()));
    } else {
        print_human(v);
    }
}

fn print_human(v: &Value) {
    match v {
        Value::Object(map) => {
            let max_key = map.keys().map(|k| k.len()).max().unwrap_or(0);
            for (k, val) in map {
                println!("  {:<width$}  {}", k, format_value(val), width = max_key);
            }
        }
        Value::Array(arr) => {
            for item in arr {
                print_human(item);
                println!();
            }
        }
        other => println!("{}", format_value(other)),
    }
}

fn format_value(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => "-".to_owned(),
        other => other.to_string(),
    }
}
