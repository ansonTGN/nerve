use std::collections::HashMap;

use crate::agent::{
    generator::{ChatResponse, SupportedFeatures, Usage},
    namespaces::ToolOutput,
    state::SharedState,
    ToolCall,
};
use anyhow::Result;
use async_trait::async_trait;
use clust::messages::{
    ClaudeModel, Content, ImageContentSource, ImageMediaType, MaxTokens, Message,
    MessagesRequestBody, Role, SystemPrompt, ToolDefinition,
};
use serde::{Deserialize, Serialize};

use super::{ChatOptions, Client};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnthropicToolFunctionParameterProperty {
    #[serde(rename(serialize = "type", deserialize = "type"))]
    pub the_type: String,
    pub description: String,
}

fn get_media_type(mime_type: &str) -> ImageMediaType {
    match mime_type {
        "image/png" => ImageMediaType::Png,
        "image/jpeg" => ImageMediaType::Jpeg,
        "image/gif" => ImageMediaType::Gif,
        "image/webp" => ImageMediaType::Webp,
        _ => ImageMediaType::Jpeg,
    }
}

pub struct AnthropicClient {
    model: ClaudeModel,
    client: clust::Client,
}

impl AnthropicClient {
    async fn get_tools_if_supported(&self, state: &SharedState) -> Vec<ToolDefinition> {
        let mut tools = vec![];

        // if native tool calls are supported (and XML was not forced)
        if state.lock().await.use_native_tools_format {
            // for every namespace available to the model
            for group in state.lock().await.get_namespaces() {
                // for every tool of the namespace
                for tool in &group.tools {
                    let mut required = vec![];
                    let mut properties = HashMap::new();

                    if let Some(example) = tool.example_payload() {
                        required.push("payload".to_string());
                        properties.insert(
                            "payload".to_string(),
                            AnthropicToolFunctionParameterProperty {
                                the_type: "string".to_string(),
                                description: format!(
                                    "The main function argument, use this as a template: {}",
                                    example
                                ),
                            },
                        );
                    }

                    if let Some(attrs) = tool.example_attributes() {
                        for name in attrs.keys() {
                            required.push(name.to_string());
                            properties.insert(
                                name.to_string(),
                                AnthropicToolFunctionParameterProperty {
                                    the_type: "string".to_string(),
                                    description: name.to_string(),
                                },
                            );
                        }
                    }

                    let input_schema = serde_json::json!({
                        "properties": properties,
                        "required": required,
                        "type": "object",
                    });

                    tools.push(ToolDefinition::new(
                        tool.name(),
                        Some(tool.description().to_string()),
                        input_schema,
                    ));
                }
            }
        }

        tools
    }
}

#[async_trait]
impl Client for AnthropicClient {
    fn new(_url: &str, _port: u16, model_name: &str, _context_window: u32) -> anyhow::Result<Self> {
        let model: ClaudeModel = if model_name.contains("opus") {
            ClaudeModel::Claude3Opus20240229
        } else if model_name.contains("sonnet") && !model_name.contains("5") {
            ClaudeModel::Claude3Sonnet20240229
        } else if model_name.contains("haiku") {
            ClaudeModel::Claude3Haiku20240307
        } else {
            ClaudeModel::Claude35Sonnet20240620
        };

        let client = clust::Client::from_env()?;
        Ok(Self { model, client })
    }

    async fn check_supported_features(&self) -> Result<SupportedFeatures> {
        let messages = vec![Message::user("Execute the test function.")];
        let max_tokens = MaxTokens::new(4096, self.model)?;

        let request_body = MessagesRequestBody {
            model: self.model,
            system: Some(SystemPrompt::new("You are an helpful assistant.")),
            messages,
            max_tokens,
            tools: Some(vec![ToolDefinition::new(
                "test",
                Some("This is a test function.".to_string()),
                serde_json::json!({
                    "properties": {},
                    "required": [],
                    "type": "object",
                }),
            )]),
            ..Default::default()
        };

        let response = self.client.create_a_message(request_body).await?;

        log::debug!("response = {:?}", response);

        if let Ok(tool_use) = response.content.flatten_into_tool_use() {
            Ok(SupportedFeatures {
                system_prompt: true,
                tools: tool_use.name == "test",
            })
        } else {
            Ok(SupportedFeatures {
                system_prompt: true,
                tools: false,
            })
        }
    }

    async fn chat(
        &self,
        state: SharedState,
        options: &ChatOptions,
    ) -> anyhow::Result<ChatResponse> {
        let mut messages = vec![Message::user(options.prompt.trim().to_string())];
        let max_tokens = MaxTokens::new(4096, self.model)?;

        for m in options.history.iter() {
            // all messages must have non-empty content except for the optional final assistant messag
            match m {
                super::Message::Agent {
                    content,
                    tool_call: _,
                } => {
                    let trimmed = content.trim();
                    if !trimmed.is_empty() {
                        messages.push(Message::assistant(trimmed))
                    } else {
                        log::warn!("ignoring empty assistant message: {:?}", m);
                    }
                }
                super::Message::Feedback {
                    result,
                    tool_call: _,
                } => match result {
                    ToolOutput::Image { data, mime_type } => messages.push(Message::user(
                        Content::from(ImageContentSource::base64(get_media_type(mime_type), data)),
                    )),
                    ToolOutput::Text(text) => {
                        let trimmed = text.trim();
                        if !trimmed.is_empty() {
                            messages.push(Message::user(trimmed))
                        } else {
                            messages.push(Message::user("no output".to_string()))
                        }
                    }
                },
            }
        }

        // if the last message is an assistant message, remove it
        if let Some(Message { role, content: _ }) = messages.last() {
            // handles "Your API request included an `assistant` message in the final position, which would pre-fill the `assistant` response"
            if matches!(role, Role::Assistant) {
                let mut logs = String::new();

                for m in messages.iter() {
                    logs.push_str(&format!("{:?}\n", m));
                }

                log::warn!("removing final assistant message for anthropic: {}", &logs);
                messages.pop();
            }
        }

        let tools = self.get_tools_if_supported(&state).await;

        let request_body = MessagesRequestBody {
            model: self.model,
            system: options
                .system_prompt
                .as_ref()
                .map(|sp| SystemPrompt::new(sp.trim())),
            messages,
            max_tokens,
            tools: if tools.is_empty() { None } else { Some(tools) },
            ..Default::default()
        };

        log::debug!("request_body = {:?}", request_body);

        let response = match self.client.create_a_message(request_body.clone()).await {
            Ok(r) => r,
            Err(e) => {
                log::error!("failed to send chat message: {e} - {:?}", request_body);
                return Err(anyhow::anyhow!("failed to send chat message: {e}"));
            }
        };

        log::debug!("response = {:?}", response);

        let content = response.content.flatten_into_text().unwrap_or_default();
        let tool_use = match response.content.flatten_into_tool_use() {
            Ok(m) => Some(m),
            Err(_) => None,
        };

        let mut tool_calls = vec![];

        log::debug!("tool_use={:?}", &tool_use);

        if let Some(tool_use) = tool_use {
            let mut attributes = HashMap::new();
            let mut argument = None;

            let object = match tool_use.input.as_object() {
                Some(o) => o,
                None => {
                    log::error!("tool_use.input is not an object: {:?}", tool_use.input);
                    return Err(anyhow::anyhow!("tool_use.input is not an object"));
                }
            };

            for (name, value) in object {
                log::debug!("tool_call.input[{}] = {:?}", name, value);

                let mut value_content = value.to_string();
                if let serde_json::Value::String(escaped_json) = &value {
                    value_content = escaped_json.to_string();
                }

                let str_val = value_content.trim_matches('"').to_string();
                if name == "payload" {
                    argument = Some(str_val);
                } else {
                    attributes.insert(name.to_string(), str_val);
                }
            }

            let tool_call = ToolCall {
                tool_name: tool_use.name.to_string(),
                named_arguments: if attributes.is_empty() {
                    None
                } else {
                    Some(attributes)
                },
                argument,
            };

            tool_calls.push(tool_call);

            log::debug!("tool_use={:?}", tool_use);
            log::debug!("tool_calls={:?}", &tool_calls);
        }

        if tool_calls.is_empty() && content.is_empty() {
            log::warn!("empty tool calls and content in response: {:?}", response);
        }

        Ok(ChatResponse {
            content: content.to_string(),
            tool_calls,
            usage: Some(Usage {
                input_tokens: response.usage.input_tokens,
                output_tokens: response.usage.output_tokens,
            }),
        })
    }
}

#[async_trait]
impl mini_rag::Embedder for AnthropicClient {
    async fn embed(&self, _text: &str) -> Result<mini_rag::Embeddings> {
        // TODO: extend the rust client to do this
        todo!("anthropic embeddings generation not yet implemented")
    }
}
