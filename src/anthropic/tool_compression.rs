use crate::kiro::model::requests::tool::{InputSchema, Tool, ToolSpecification};

const MIN_DESCRIPTION_CHARS: usize = 50;

struct CountingWriter(usize);

impl std::io::Write for CountingWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0 += buf.len();
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

pub fn compress_tools_if_needed(tools: &[Tool], max_bytes: usize) -> Vec<Tool> {
    if max_bytes == 0 {
        return tools.to_vec();
    }

    let total_size = estimate_tools_size(tools);
    if total_size <= max_bytes {
        return tools.to_vec();
    }

    let mut compressed: Vec<Tool> = tools.iter().map(simplify_schema).collect();
    let size_after_schema = estimate_tools_size(&compressed);
    if size_after_schema == 0 || size_after_schema <= max_bytes {
        return compressed;
    }

    let ratio = max_bytes as f64 / size_after_schema as f64;
    for tool in &mut compressed {
        let desc = &tool.tool_specification.description;
        let target_bytes = (desc.len() as f64 * ratio) as usize;
        let min_bytes = desc
            .char_indices()
            .nth(MIN_DESCRIPTION_CHARS)
            .map(|(idx, _)| idx)
            .unwrap_or(desc.len());
        let target_bytes = target_bytes.max(min_bytes);
        if desc.len() > target_bytes {
            let truncate_at = desc
                .char_indices()
                .take_while(|(idx, _)| *idx <= target_bytes)
                .last()
                .map(|(idx, ch)| idx + ch.len_utf8())
                .unwrap_or(0);
            tool.tool_specification.description = desc[..truncate_at].to_string();
        }
    }

    compressed
}

pub(crate) fn estimate_tools_size(tools: &[Tool]) -> usize {
    tools
        .iter()
        .map(|tool| {
            let spec = &tool.tool_specification;
            let mut writer = CountingWriter(0);
            let schema_len = serde_json::to_writer(&mut writer, &spec.input_schema.json)
                .map(|_| writer.0)
                .unwrap_or(0);
            spec.name.len() + spec.description.len() + schema_len
        })
        .sum()
}

fn simplify_schema(tool: &Tool) -> Tool {
    Tool {
        tool_specification: ToolSpecification {
            name: tool.tool_specification.name.clone(),
            description: tool.tool_specification.description.clone(),
            input_schema: InputSchema::from_json(simplify_json_schema(
                &tool.tool_specification.input_schema.json,
            )),
        },
    }
}

fn simplify_json_schema(schema: &serde_json::Value) -> serde_json::Value {
    let Some(obj) = schema.as_object() else {
        return schema.clone();
    };

    let mut result = serde_json::Map::new();
    for (key, value) in obj {
        if key == "description" {
            continue;
        }

        let simplified = match key.as_str() {
            "properties" | "patternProperties" | "$defs" | "definitions" | "dependentSchemas" => {
                simplify_schema_map(value)
            }
            "oneOf" | "anyOf" | "allOf" | "prefixItems" => simplify_schema_array(value),
            "items"
            | "additionalProperties"
            | "additionalItems"
            | "unevaluatedProperties"
            | "unevaluatedItems"
            | "not"
            | "if"
            | "then"
            | "else"
            | "propertyNames"
            | "contains" => simplify_schema_or_clone(value),
            "dependencies" => simplify_dependencies(value),
            _ => value.clone(),
        };

        result.insert(key.clone(), simplified);
    }

    serde_json::Value::Object(result)
}

fn simplify_schema_or_clone(value: &serde_json::Value) -> serde_json::Value {
    if value.is_object() {
        simplify_json_schema(value)
    } else {
        value.clone()
    }
}

fn simplify_schema_array(value: &serde_json::Value) -> serde_json::Value {
    let Some(values) = value.as_array() else {
        return value.clone();
    };

    serde_json::Value::Array(values.iter().map(simplify_json_schema).collect())
}

fn simplify_schema_map(value: &serde_json::Value) -> serde_json::Value {
    let Some(values) = value.as_object() else {
        return value.clone();
    };

    let mut simplified = serde_json::Map::new();
    for (name, schema) in values {
        simplified.insert(name.clone(), simplify_json_schema(schema));
    }

    serde_json::Value::Object(simplified)
}

fn simplify_dependencies(value: &serde_json::Value) -> serde_json::Value {
    let Some(values) = value.as_object() else {
        return value.clone();
    };

    let mut simplified = serde_json::Map::new();
    for (name, dependency) in values {
        simplified.insert(name.clone(), simplify_schema_or_clone(dependency));
    }

    serde_json::Value::Object(simplified)
}

#[cfg(test)]
mod tests {
    use crate::kiro::model::requests::tool::{InputSchema, Tool, ToolSpecification};

    fn make_tool(name: &str, description: &str, schema: serde_json::Value) -> Tool {
        Tool {
            tool_specification: ToolSpecification {
                name: name.to_string(),
                description: description.to_string(),
                input_schema: InputSchema::from_json(schema),
            },
        }
    }

    #[test]
    fn compress_tools_removes_schema_descriptions_when_over_threshold() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "very long parameter description"
                }
            },
            "required": ["path"]
        });
        let tools: Vec<_> = (0..20)
            .map(|idx| make_tool(&format!("tool_{idx}"), &"x".repeat(2_000), schema.clone()))
            .collect();

        let original_size = serde_json::to_string(&tools).unwrap().len();
        let compressed = super::compress_tools_if_needed(&tools, 20 * 1024);
        let compressed_size = serde_json::to_string(&compressed).unwrap().len();

        assert!(compressed_size < original_size);
        let path_schema = &compressed[0].tool_specification.input_schema.json["properties"]["path"];
        assert!(path_schema.get("description").is_none());
        assert_eq!(path_schema["type"], "string");
    }

    #[test]
    fn compress_tools_preserves_polymorphic_schema_keywords() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "target": {
                    "oneOf": [
                        {
                            "type": "string",
                            "description": "path-like target"
                        },
                        {
                            "type": "object",
                            "properties": {
                                "id": {
                                    "type": "string",
                                    "description": "resource id"
                                }
                            },
                            "required": ["id"]
                        }
                    ],
                    "description": "polymorphic target"
                }
            },
            "required": ["target"]
        });
        let tools: Vec<_> = (0..20)
            .map(|idx| make_tool(&format!("tool_{idx}"), &"x".repeat(2_000), schema.clone()))
            .collect();

        let compressed = super::compress_tools_if_needed(&tools, 20 * 1024);
        let target = &compressed[0].tool_specification.input_schema.json["properties"]["target"];

        assert!(target.get("oneOf").is_some());
        assert!(
            target["oneOf"][0]
                .as_object()
                .unwrap()
                .get("description")
                .is_none()
        );
        assert_eq!(target["oneOf"][1]["properties"]["id"]["type"], "string");
        assert!(
            target["oneOf"][1]["properties"]["id"]
                .as_object()
                .unwrap()
                .get("description")
                .is_none()
        );
    }

    #[test]
    fn compress_tools_preserves_validation_schema_keywords() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "minLength": 1,
                    "maxLength": 512,
                    "pattern": "^/",
                    "format": "uri-reference",
                    "default": "/tmp/input.txt",
                    "description": "absolute path"
                },
                "count": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 100,
                    "multipleOf": 1,
                    "description": "bounded count"
                },
                "tags": {
                    "type": "array",
                    "items": {"type": "string", "minLength": 1},
                    "minItems": 1,
                    "maxItems": 10,
                    "uniqueItems": true,
                    "prefixItems": [{"type": "string", "const": "primary"}],
                    "description": "bounded tags"
                }
            },
            "patternProperties": {
                "^x-": {
                    "type": "string",
                    "maxLength": 20,
                    "description": "custom extension"
                }
            },
            "required": ["path"]
        });
        let tools: Vec<_> = (0..20)
            .map(|idx| make_tool(&format!("tool_{idx}"), &"x".repeat(2_000), schema.clone()))
            .collect();

        let compressed = super::compress_tools_if_needed(&tools, 20 * 1024);
        let json = &compressed[0].tool_specification.input_schema.json;

        assert_eq!(json["properties"]["path"]["minLength"], 1);
        assert_eq!(json["properties"]["path"]["maxLength"], 512);
        assert_eq!(json["properties"]["path"]["pattern"], "^/");
        assert_eq!(json["properties"]["path"]["format"], "uri-reference");
        assert_eq!(json["properties"]["path"]["default"], "/tmp/input.txt");
        assert_eq!(json["properties"]["count"]["minimum"], 1);
        assert_eq!(json["properties"]["count"]["maximum"], 100);
        assert_eq!(json["properties"]["count"]["multipleOf"], 1);
        assert_eq!(json["properties"]["tags"]["minItems"], 1);
        assert_eq!(json["properties"]["tags"]["maxItems"], 10);
        assert_eq!(json["properties"]["tags"]["uniqueItems"], true);
        assert_eq!(
            json["properties"]["tags"]["prefixItems"][0]["const"],
            "primary"
        );
        assert_eq!(json["patternProperties"]["^x-"]["maxLength"], 20);
        assert!(
            json["properties"]["path"]
                .as_object()
                .unwrap()
                .get("description")
                .is_none()
        );
    }

    #[test]
    fn compress_tools_preserves_schema_references() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "target": {
                    "$ref": "#/$defs/Target",
                    "description": "referenced target"
                }
            },
            "$defs": {
                "Target": {
                    "type": "object",
                    "properties": {
                        "id": {
                            "type": "string",
                            "description": "target id"
                        }
                    },
                    "required": ["id"],
                    "description": "target definition"
                }
            },
            "definitions": {
                "LegacyTarget": {
                    "type": "string",
                    "description": "legacy target"
                }
            },
            "required": ["target"]
        });
        let tools: Vec<_> = (0..20)
            .map(|idx| make_tool(&format!("tool_{idx}"), &"x".repeat(2_000), schema.clone()))
            .collect();

        let compressed = super::compress_tools_if_needed(&tools, 20 * 1024);
        let json = &compressed[0].tool_specification.input_schema.json;

        assert_eq!(json["properties"]["target"]["$ref"], "#/$defs/Target");
        assert_eq!(json["$defs"]["Target"]["type"], "object");
        assert_eq!(
            json["$defs"]["Target"]["properties"]["id"]["type"],
            "string"
        );
        assert_eq!(json["definitions"]["LegacyTarget"]["type"], "string");
        assert!(
            json["$defs"]["Target"]["properties"]["id"]
                .as_object()
                .unwrap()
                .get("description")
                .is_none()
        );
    }

    #[test]
    fn compress_tools_preserves_conditional_and_dependency_schema_keywords() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "mode": {
                    "type": "string",
                    "enum": ["file", "url"],
                    "description": "input mode"
                }
            },
            "if": {
                "properties": {
                    "mode": {"const": "file"}
                },
                "description": "file mode condition"
            },
            "then": {
                "required": ["path"],
                "description": "file mode requirements"
            },
            "else": {
                "required": ["url"],
                "description": "url mode requirements"
            },
            "not": {
                "required": ["forbidden"],
                "description": "forbidden field"
            },
            "propertyNames": {
                "pattern": "^[a-z_]+$",
                "description": "property name pattern"
            },
            "contains": {
                "type": "string",
                "description": "array contains schema"
            },
            "dependentSchemas": {
                "token": {
                    "required": ["auth"],
                    "description": "token dependency schema"
                }
            },
            "dependencies": {
                "legacy": {
                    "required": ["legacy_id"],
                    "description": "legacy dependency schema"
                },
                "mode": ["path"]
            },
            "dependentRequired": {
                "mode": ["path"]
            }
        });
        let tools: Vec<_> = (0..20)
            .map(|idx| make_tool(&format!("tool_{idx}"), &"x".repeat(2_000), schema.clone()))
            .collect();

        let compressed = super::compress_tools_if_needed(&tools, 20 * 1024);
        let json = &compressed[0].tool_specification.input_schema.json;

        assert_eq!(json["if"]["properties"]["mode"]["const"], "file");
        assert_eq!(json["then"]["required"][0], "path");
        assert_eq!(json["else"]["required"][0], "url");
        assert_eq!(json["not"]["required"][0], "forbidden");
        assert_eq!(json["propertyNames"]["pattern"], "^[a-z_]+$");
        assert_eq!(json["contains"]["type"], "string");
        assert_eq!(json["dependentSchemas"]["token"]["required"][0], "auth");
        assert_eq!(json["dependencies"]["legacy"]["required"][0], "legacy_id");
        assert_eq!(json["dependencies"]["mode"][0], "path");
        assert_eq!(json["dependentRequired"]["mode"][0], "path");
        assert!(
            json["dependentSchemas"]["token"]
                .as_object()
                .unwrap()
                .get("description")
                .is_none()
        );
    }

    #[test]
    fn compress_tools_handles_zero_estimated_schema_size() {
        let tools = vec![make_tool("", "", serde_json::Value::Null)];

        let compressed = super::compress_tools_if_needed(&tools, 1);

        assert_eq!(compressed[0].tool_specification.name, "");
        assert_eq!(compressed[0].tool_specification.description, "");
        assert_eq!(
            compressed[0].tool_specification.input_schema.json,
            serde_json::Value::Null
        );
    }

    #[test]
    fn compress_tools_returns_original_under_threshold() {
        let tools = vec![make_tool(
            "small",
            "short description",
            serde_json::json!({"type": "object", "properties": {}}),
        )];

        let compressed = super::compress_tools_if_needed(&tools, 20 * 1024);

        assert_eq!(
            compressed[0].tool_specification.description,
            "short description"
        );
    }
}
