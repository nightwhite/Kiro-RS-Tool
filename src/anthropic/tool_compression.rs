use crate::kiro::model::requests::tool::{InputSchema, Tool, ToolSpecification};

const MIN_DESCRIPTION_CHARS: usize = 50;

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
    if size_after_schema <= max_bytes {
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

fn estimate_tools_size(tools: &[Tool]) -> usize {
    tools
        .iter()
        .map(|tool| {
            let spec = &tool.tool_specification;
            spec.name.len()
                + spec.description.len()
                + serde_json::to_string(&spec.input_schema.json)
                    .map(|value| value.len())
                    .unwrap_or(0)
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
    for key in ["$schema", "type", "required", "additionalProperties"] {
        if let Some(value) = obj.get(key) {
            result.insert(key.to_string(), value.clone());
        }
    }

    if let Some(serde_json::Value::Object(props)) = obj.get("properties") {
        let mut simplified_props = serde_json::Map::new();
        for (name, prop_schema) in props {
            if let Some(prop_obj) = prop_schema.as_object() {
                let mut simplified_prop = serde_json::Map::new();
                if let Some(value) = prop_obj.get("type") {
                    simplified_prop.insert("type".to_string(), value.clone());
                }
                if let Some(value) = prop_obj.get("enum") {
                    simplified_prop.insert("enum".to_string(), value.clone());
                }
                if let Some(value) = prop_obj.get("items") {
                    simplified_prop.insert("items".to_string(), simplify_json_schema(value));
                }
                if let Some(nested_props) = prop_obj.get("properties") {
                    let mut nested_schema = serde_json::Map::new();
                    nested_schema.insert(
                        "type".to_string(),
                        serde_json::Value::String("object".to_string()),
                    );
                    nested_schema.insert("properties".to_string(), nested_props.clone());
                    if let Some(value) = prop_obj.get("required") {
                        nested_schema.insert("required".to_string(), value.clone());
                    }
                    if let Some(value) = prop_obj.get("additionalProperties") {
                        nested_schema.insert("additionalProperties".to_string(), value.clone());
                    }
                    let nested = simplify_json_schema(&serde_json::Value::Object(nested_schema));
                    if let Some(value) = nested.get("properties") {
                        simplified_prop.insert("properties".to_string(), value.clone());
                    }
                    if let Some(value) = nested.get("required") {
                        simplified_prop.insert("required".to_string(), value.clone());
                    }
                    if let Some(value) = nested.get("additionalProperties") {
                        simplified_prop.insert("additionalProperties".to_string(), value.clone());
                    }
                }
                simplified_props.insert(name.clone(), serde_json::Value::Object(simplified_prop));
            } else {
                simplified_props.insert(name.clone(), prop_schema.clone());
            }
        }
        result.insert(
            "properties".to_string(),
            serde_json::Value::Object(simplified_props),
        );
    }

    serde_json::Value::Object(result)
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
