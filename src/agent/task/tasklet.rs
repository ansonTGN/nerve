use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::str::FromStr;
use std::{collections::HashMap, time::Duration};

use anyhow::Result;
use async_trait::async_trait;
use colored::Colorize;
use duration_string::DurationString;
use serde::{Deserialize, Serialize};
use serde_trim::*;

use super::Evaluator;
use super::{variables::interpolate_variables, Task};
use crate::agent::namespaces::ToolOutput;
use crate::agent::task::robopages;
use crate::agent::task::variables::define_variable;
use crate::agent::{get_user_input, namespaces};
use crate::agent::{
    namespaces::{Namespace, Tool},
    state::SharedState,
    task::variables::{parse_pre_defined_values, parse_variable_expr},
};

const STATE_COMPLETE_EXIT_CODE: i32 = 65;

fn default_max_shown_output() -> usize {
    256
}

#[derive(Default, Deserialize, Serialize, Debug, Clone)]
pub struct TaskletTool {
    #[serde(skip_deserializing, skip_serializing)]
    working_directory: String,
    #[serde(skip_deserializing, skip_serializing)]
    robopages_server_address: Option<String>,

    #[serde(default = "default_max_shown_output")]
    max_shown_output: usize,
    #[serde(deserialize_with = "string_trim")]
    name: String,
    #[serde(deserialize_with = "string_trim")]
    description: String,
    args: Option<HashMap<String, String>>,
    define: Option<HashMap<String, String>>,
    store_to: Option<String>,
    example_payload: Option<String>,
    timeout: Option<String>,
    mime_type: Option<String>,

    judge: Option<String>,
    #[serde(skip_deserializing, skip_serializing)]
    judge_path: Option<PathBuf>,

    complete_task: Option<bool>,
    ignore_stderr: Option<bool>,

    tool: Option<String>,

    alias: Option<String>,
    #[serde(skip_deserializing, skip_serializing)]
    aliased_to: Option<Box<dyn Tool>>,
}

impl TaskletTool {
    async fn run_via_robopages(
        &self,
        attributes: Option<HashMap<String, String>>,
    ) -> Result<Option<String>> {
        let result =
            robopages::Client::new(self.robopages_server_address.as_ref().unwrap().clone())
                .execute(&self.name, attributes.unwrap_or_default())
                .await?;
        Ok(Some(result))
    }

    async fn run_as_judge(&self, payload: Option<String>) -> Result<Option<String>> {
        let nerve_exe = std::env::current_exe()
            .map_err(|e| anyhow!("Failed to get current executable path: {}", e))?;

        let generator = std::env::var("NERVE_JUDGE").unwrap();

        let mut cmd = Command::new(&nerve_exe);
        cmd.args([
            "-J",
            &generator,
            "-T",
            self.judge_path.as_ref().unwrap().to_str().unwrap(),
            "--judge-mode",
        ]);

        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("Failed to spawn judge child process");

        log::info!("running judge: {:?}", &cmd);

        let mut stdin = child.stdin.take().expect("Failed to open stdin");
        std::thread::spawn(move || {
            stdin
                .write_all(format!("{}\n", payload.unwrap()).as_bytes())
                .expect("Failed to write to judge stdin");
        });

        // let output = cmd.output();
        let output = child.wait_with_output();
        let mut result = String::new();

        if let Ok(output) = output {
            let err = String::from_utf8_lossy(&output.stderr).trim().to_string();
            let out = String::from_utf8_lossy(&output.stdout).trim().to_string();

            if !err.is_empty() {
                result = format!("ERROR: {}\n", err);
            }

            if !out.is_empty() {
                result = out.to_string();
            }
        } else {
            result = format!("ERROR: {}", output.err().unwrap());
        }

        log::info!("  > {}", &result);

        Ok(Some(result))
    }

    async fn run_as_command_line(
        &self,
        state: SharedState,
        attributes: Option<HashMap<String, String>>,
        payload: Option<String>,
    ) -> Result<Option<String>> {
        // run as local tool
        let parts: Vec<String> = self
            .tool
            .as_ref()
            .ok_or_else(|| anyhow!("tool not set"))?
            .split_whitespace()
            .map(|x| x.trim())
            .filter(|x| !x.is_empty())
            .map(|x| x.to_string())
            .collect();

        if parts.is_empty() {
            return Err(anyhow!("no tool defined"));
        }

        // TODO: each tool should have its own variables ... ?

        // define tool specific variables
        if let Some(payload) = &payload {
            define_variable("PAYLOAD", payload);
        }

        if let Some(attributes) = &attributes {
            for (key, value) in attributes {
                define_variable(&format!("ATTRIBUTES.{}", key), value);
            }
        }

        let mut payload_consumed = false;
        let mut cmd = Command::new(&parts[0]);
        if parts.len() > 1 {
            // more complex command line
            for part in &parts[1..] {
                if part.as_bytes()[0] == b'$' {
                    let (var_name, var_value) = parse_variable_expr(part).await?;
                    if var_name == "PAYLOAD" {
                        payload_consumed = true;
                    }
                    cmd.arg(var_value);
                } else {
                    // raw value
                    cmd.arg(part);
                }
            }
        }

        cmd.current_dir(&self.working_directory);

        if let Some(attrs) = &attributes {
            for (key, value) in attrs {
                cmd.args([&format!("--{}", key), value]);
            }
        }

        // if the payload was not excplicitly referenced by $PAYLOAD, add it as the last argument
        if !payload_consumed {
            if let Some(payload) = &payload {
                cmd.arg(payload);
            }
        }

        log::debug!("! {:?}", &cmd);

        let output = cmd.output();
        if let Ok(output) = output {
            let err = String::from_utf8_lossy(&output.stderr).trim().to_string();
            let out = String::from_utf8_lossy(&output.stdout).trim().to_string();

            // do not log output if mime type is set
            if self.mime_type.is_none() {
                if !err.is_empty()
                    && self.max_shown_output > 0
                    && !self.ignore_stderr.unwrap_or(false)
                {
                    log::error!(
                        "{}",
                        if err.len() > self.max_shown_output {
                            format!(
                                "{}\n{}",
                                &err[0..self.max_shown_output].red(),
                                "... truncated ...".yellow()
                            )
                        } else {
                            err.red().to_string()
                        }
                    );
                }

                if !out.is_empty() && self.max_shown_output > 0 {
                    let lines = if out.len() > self.max_shown_output {
                        let end = out
                            .char_indices()
                            .map(|(i, _)| i)
                            .nth(self.max_shown_output)
                            .unwrap();
                        let ascii = &out[0..end];
                        format!("{}\n{}", ascii, "... truncated ...")
                    } else {
                        out.to_string()
                    }
                    .split('\n')
                    .map(|s| s.dimmed().to_string())
                    .collect::<Vec<String>>();

                    for line in lines {
                        log::info!("{}", line);
                    }
                }
            }

            let exit_code = output.status.code().unwrap_or(0);
            log::debug!("exit_code={}", exit_code);
            if exit_code == STATE_COMPLETE_EXIT_CODE {
                state
                    .lock()
                    .await
                    .on_complete(false, Some(out.clone()))
                    .await?;
                return Ok(Some("task complete".into()));
            }

            if !err.is_empty() && !self.ignore_stderr.unwrap_or(false) {
                Err(anyhow!(err))
            } else {
                Ok(Some(out))
            }
        } else {
            let err = output.err().unwrap().to_string();
            log::error!("{}", &err);
            Err(anyhow!(err))
        }
    }
}

#[async_trait]
impl Tool for TaskletTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn example_payload(&self) -> Option<&str> {
        if self.example_payload.is_some() {
            return self.example_payload.as_deref();
        } else if let Some(aliased_to) = &self.aliased_to {
            return aliased_to.example_payload();
        }
        None
    }

    fn example_attributes(&self) -> Option<HashMap<String, String>> {
        if self.args.is_some() {
            return self.args.clone();
        } else if let Some(aliased_to) = &self.aliased_to {
            return aliased_to.example_attributes();
        }
        None
    }

    fn timeout(&self) -> Option<Duration> {
        if let Some(timeout) = &self.timeout {
            if let Ok(tm) = timeout.parse::<DurationString>() {
                return Some(*tm);
            } else {
                log::error!("can't parse '{}' as duration string", timeout);
            }
        } else if let Some(aliased_to) = &self.aliased_to {
            return aliased_to.timeout();
        }
        None
    }

    fn complete_task(&self) -> bool {
        self.complete_task.unwrap_or(false)
    }

    async fn run(
        &self,
        state: SharedState,
        attributes: Option<HashMap<String, String>>,
        payload: Option<String>,
    ) -> Result<Option<ToolOutput>> {
        let tool_output = if let Some(aliased_to) = &self.aliased_to {
            // run as alias of a builtin namespace.tool, here we can unwrap as everything is validated earlier
            aliased_to.run(state.clone(), attributes, payload).await?
        } else {
            let output = if self.robopages_server_address.is_some() {
                // run via robopages server
                self.run_via_robopages(attributes).await?
            } else if self.judge_path.is_some() {
                // run as another instance of nerve for the judge tool
                self.run_as_judge(payload).await?
            } else if self.tool.is_some() {
                // run as a command line tool
                self.run_as_command_line(state.clone(), attributes, payload)
                    .await?
            } else {
                // just return the payload
                payload
            };

            if let Some(output) = output {
                if let Some(mime_type) = &self.mime_type {
                    Some(ToolOutput::image(output, mime_type.to_string()))
                } else {
                    Some(ToolOutput::text(output))
                }
            } else {
                None
            }
        };

        if let Some(store_to) = &self.store_to {
            log::debug!("storing output to {}", store_to);

            state.lock().await.set_variable(
                store_to.to_string(),
                if let Some(output) = &tool_output {
                    output.to_string()
                } else {
                    "".to_string()
                },
            );
        }

        Ok(tool_output)
    }
}

#[derive(Default, Deserialize, Serialize, Debug, Clone)]
pub struct ToolBox {
    #[serde(deserialize_with = "string_trim")]
    pub name: String,
    pub description: Option<String>,

    // for backwards compatibility
    #[serde(alias = "actions")]
    pub tools: Vec<TaskletTool>,
}

impl ToolBox {
    fn compile(
        &self,
        working_directory: &str,
        robopages_server_address: Option<String>,
    ) -> Result<Namespace> {
        let mut tools: Vec<Box<dyn Tool>> = vec![];
        for tasklet_tool in &self.tools {
            let mut tool = tasklet_tool.clone();
            tool.working_directory = working_directory.to_string();
            tool.robopages_server_address = robopages_server_address.clone();
            tools.push(Box::new(tool));
        }

        Ok(Namespace::new_default(
            self.name.to_string(),
            if let Some(desc) = &self.description {
                desc.to_string()
            } else {
                "".to_string()
            },
            tools,
            None, // TODO: let tasklets declare custom storages?
        ))
    }
}

#[derive(Default, Deserialize, Serialize, Debug, Clone)]
pub struct Tasklet {
    #[serde(skip_deserializing, skip_serializing)]
    pub folder: String,
    #[serde(skip_deserializing)]
    pub name: String,
    #[serde(deserialize_with = "string_trim")]
    system_prompt: String,
    pub prompt: Option<String>,
    pub rag: Option<mini_rag::Configuration>,
    timeout: Option<String>,
    using: Option<Vec<String>>,
    guidance: Option<Vec<String>>,

    // for backwards compatibility
    #[serde(alias = "functions")]
    tool_box: Option<Vec<ToolBox>>,

    evaluator: Option<Evaluator>,

    #[serde(skip_deserializing, skip_serializing)]
    robopages: Vec<ToolBox>,
    #[serde(skip_deserializing, skip_serializing)]
    robopages_server_address: Option<String>,
}

impl Tasklet {
    pub async fn from_path(tasklet_path: &str, defines: &Vec<String>) -> Result<Self> {
        parse_pre_defined_values(defines)?;

        let mut tasklet_path = PathBuf::from_str(tasklet_path)?;
        // try to look it up in ~/.nerve/tasklets
        if !tasklet_path.exists() {
            let in_home = crate::agent::data_path("tasklets")?.join(&tasklet_path);
            if in_home.exists() {
                tasklet_path = in_home;
            }
        }

        if tasklet_path.is_dir() {
            Self::from_folder(tasklet_path.to_str().unwrap()).await
        } else {
            Self::from_yaml_file(tasklet_path.to_str().unwrap()).await
        }
    }

    async fn from_folder(path: &str) -> Result<Self> {
        let filepath = PathBuf::from_str(path);
        if let Err(err) = filepath {
            Err(anyhow!("could not read {path}: {err}"))
        } else {
            Self::from_yaml_file(filepath.unwrap().join("task.yml").to_str().unwrap()).await
        }
    }

    async fn from_yaml_file(filepath: &str) -> Result<Self> {
        let canon = std::fs::canonicalize(filepath);
        if let Err(err) = canon {
            Err(anyhow!("could not read {filepath}: {err}"))
        } else {
            let canon = canon.unwrap();
            let tasklet_parent_folder = if let Some(folder) = canon.parent() {
                folder
            } else {
                return Err(anyhow!("can't find parent folder of {}", canon.display()));
            };

            // read the tasklet contents
            let yaml = std::fs::read_to_string(&canon)?;

            // preprocess the tasklet contents by interpolating $file and
            // $http/$https schema variables in order to avoid doing it at
            // every agent step. Other variables will be left untouched and
            // resolved at runtime.
            let yaml = crate::agent::task::variables::preprocess_content(&yaml).await?;

            // parse the yaml
            let mut tasklet: Self = serde_yaml::from_str(&yaml)?;

            // used to set the working directory while running the task
            tasklet.folder = if let Some(folder) = tasklet_parent_folder.to_str() {
                folder.to_string()
            } else {
                return Err(anyhow!("can't get string of {:?}", tasklet_parent_folder));
            };

            // set unique task name from the folder or yaml file itself
            tasklet.name = if canon.ends_with("task.yaml") || canon.ends_with("task.yml") {
                tasklet_parent_folder
                    .file_name()
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .to_owned()
            } else {
                canon.file_stem().unwrap().to_str().unwrap().to_owned()
            };

            // check any tool definied as alias of a builtin namespace and perform some preprocessing and validation
            if let Some(functions) = tasklet.tool_box.as_mut() {
                // for each group of functions
                for group in functions {
                    // for each tool in the group
                    for tool in &mut group.tools {
                        if let Some(defines) = &tool.define {
                            for (key, value) in defines {
                                log::debug!(
                                    "defining variable {} = '{}' for {}.{}",
                                    key,
                                    value,
                                    group.name,
                                    tool.name
                                );
                                define_variable(key, value);
                            }
                        }

                        // if the tool is a judge, validate the judge file
                        if let Some(judge) = &tool.judge {
                            let judge_path = if judge.starts_with('/') {
                                PathBuf::from_str(judge)?
                            } else {
                                PathBuf::from(&tasklet.folder).join(judge)
                            };
                            if !judge_path.exists() {
                                return Err(anyhow!(
                                    "judge file '{}' not found",
                                    judge_path.display()
                                ));
                            } else if !judge_path.is_file() {
                                return Err(anyhow!(
                                    "judge file '{}' is not a file",
                                    judge_path.display()
                                ));
                            }
                            tool.judge_path = Some(judge_path);
                        }

                        // if the tool has an alias perform some validation
                        if let Some(alias) = &tool.alias {
                            if tool.tool.is_some() {
                                return Err(anyhow!("can't define both tool and alias"));
                            }

                            let (namespace_name, tool_name) = alias
                                .split_once('.')
                                .ok_or_else(|| anyhow!("invalid alias format '{}', aliases must be provided as 'namespace.tool'", alias))?;

                            if let Some(get_namespace_fn) =
                                namespaces::NAMESPACES.get(namespace_name)
                            {
                                let le_namespace = get_namespace_fn();
                                let le_tool =
                                    le_namespace.tools.iter().find(|a| a.name() == tool_name);

                                if let Some(le_tool) = le_tool {
                                    log::debug!(
                                        "aliased {}.{} to {}.{}",
                                        group.name,
                                        tool.name,
                                        le_namespace.name,
                                        le_tool.name()
                                    );
                                    tool.aliased_to = Some(le_tool.clone());
                                } else {
                                    return Err(anyhow!(
                                        "tool '{}' not found in namespace '{}'",
                                        tool_name,
                                        namespace_name
                                    ));
                                }
                            } else {
                                return Err(anyhow!("namespace '{}' not found", namespace_name));
                            }
                        }
                    }
                }
            }

            log::debug!("tasklet = {:?}", &tasklet);

            Ok(tasklet)
        }
    }

    pub async fn prepare(&mut self, user_prompt: &Option<String>) -> Result<()> {
        if self.prompt.is_none() {
            self.prompt = Some(if let Some(prompt) = &user_prompt {
                // if passed by command line
                prompt.to_string()
            } else {
                // ask the user
                get_user_input("enter task> ")
            });
        }

        // parse any variable
        self.prompt = Some(interpolate_variables(self.prompt.as_ref().unwrap().trim()).await?);

        // fix paths
        if let Some(rag) = self.rag.as_mut() {
            let src_path = PathBuf::from(&rag.source_path);
            if src_path.is_relative() {
                rag.source_path =
                    std::fs::canonicalize(PathBuf::from(&self.folder).join(src_path))?
                        .display()
                        .to_string();
            }

            let data_path = PathBuf::from(&rag.data_path);
            if data_path.is_relative() {
                rag.data_path = std::fs::canonicalize(PathBuf::from(&self.folder).join(data_path))?
                    .display()
                    .to_string();
            }
        }

        Ok(())
    }

    pub fn set_robopages(&mut self, server_address: &str, robopages: Vec<ToolBox>) {
        let mut host_port = if server_address.contains("://") {
            server_address.split("://").last().unwrap().to_string()
        } else {
            server_address.to_string()
        };

        if host_port.contains("/") {
            host_port = host_port.split('/').next().unwrap().to_string();
        }

        self.robopages_server_address = Some(host_port);
        self.robopages = robopages;
    }
}

impl Task for Tasklet {
    fn get_timeout(&self) -> Option<std::time::Duration> {
        if let Some(timeout) = &self.timeout {
            if let Ok(tm) = timeout.parse::<DurationString>() {
                return Some(*tm);
            } else {
                log::error!("can't parse '{}' as duration string", timeout);
            }
        }
        None
    }

    fn get_working_directory(&self) -> Option<String> {
        Some(self.folder.clone())
    }

    fn get_evaluator(&self) -> Option<Evaluator> {
        self.evaluator.clone()
    }

    fn get_rag_config(&self) -> Option<mini_rag::Configuration> {
        self.rag.clone()
    }

    fn to_system_prompt(&self) -> Result<String> {
        Ok(self.system_prompt.to_string())
    }

    fn to_prompt(&self) -> Result<String> {
        if let Some(prompt) = &self.prompt {
            Ok(prompt.to_string())
        } else {
            Err(anyhow!("prompt not specified"))
        }
    }

    fn namespaces(&self) -> Option<Vec<String>> {
        self.using.clone()
    }

    fn guidance(&self) -> Result<Vec<String>> {
        let base = self.base_guidance()?;
        // extend the set of basic rules
        Ok([base, self.guidance.as_ref().unwrap_or(&vec![]).clone()].concat())
    }

    fn get_functions(&self) -> Vec<Namespace> {
        let mut groups = vec![];

        if let Some(custom_functions) = self.tool_box.as_ref() {
            for group in custom_functions {
                groups.push(group.compile(&self.folder, None).unwrap());
            }
        }

        if !self.robopages.is_empty() {
            for group in &self.robopages {
                groups.push(
                    group
                        .compile(&self.folder, self.robopages_server_address.clone())
                        .unwrap(),
                );
            }
        }

        groups
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::task::variables::define_variable;
    use std::fs;
    use tempfile::tempdir;

    #[tokio::test]
    async fn test_tasklet_preprocessing_should_not_interpolate_variables() -> Result<()> {
        let temp_dir = tempdir()?;
        let task_dir = temp_dir.path().join("test_task");
        fs::create_dir(&task_dir)?;

        // Create a task.yml with variables
        let task_yaml = r#"
system_prompt: "Inline variables $VAR1"
prompt: "Should not be preprocessed $VAR2"
"#;
        fs::write(task_dir.join("task.yml"), task_yaml)?;

        // Define variables before loading
        define_variable("VAR1", "value1");
        define_variable("VAR2", "value2");

        let tasklet = Tasklet::from_yaml_file(task_dir.join("task.yml").to_str().unwrap()).await?;

        assert_eq!(tasklet.to_system_prompt()?, "Inline variables $VAR1");
        assert_eq!(tasklet.to_prompt()?, "Should not be preprocessed $VAR2");

        Ok(())
    }

    #[tokio::test]
    async fn test_tasklet_preprocessing_should_interpolate_file_variables() -> Result<()> {
        let temp_dir = tempfile::tempdir().unwrap();
        let task_path = temp_dir.path().join("task.yml");

        let file_path = temp_dir.path().join("test.txt");
        std::fs::write(&file_path, "hello from file").unwrap();

        // Create task.yml referencing the file
        let task_yaml = format!(
            r#"
system_prompt: "System prompt with $file://{}"
prompt: "Regular prompt"
"#,
            file_path.display()
        );
        fs::write(&task_path, &task_yaml)?;

        let tasklet = Tasklet::from_yaml_file(task_path.to_str().unwrap()).await?;

        assert_eq!(
            tasklet.to_system_prompt()?,
            "System prompt with hello from file"
        );
        assert_eq!(tasklet.to_prompt()?, "Regular prompt");

        Ok(())
    }

    #[tokio::test]
    async fn test_tasklet_preprocessing_should_interpolate_missing_file_variables_with_default(
    ) -> Result<()> {
        let temp_dir = tempdir()?;
        let task_dir = temp_dir.path().join("test_task");
        fs::create_dir(&task_dir)?;

        // Create task.yml referencing non-existent file with default
        let task_yaml = r#"
system_prompt: "System prompt with $file:///nonexistent.txt||fallback"
prompt: "Regular prompt"
"#;
        fs::write(task_dir.join("task.yml"), task_yaml)?;

        let tasklet = Tasklet::from_yaml_file(task_dir.join("task.yml").to_str().unwrap()).await?;

        assert_eq!(tasklet.to_system_prompt()?, "System prompt with fallback");
        assert_eq!(tasklet.to_prompt()?, "Regular prompt");

        Ok(())
    }

    #[tokio::test]
    async fn test_tasklet_preprocessing_should_interpolate_tool_box_from_file() -> Result<()> {
        let temp_dir = tempdir()?;
        let task_dir = temp_dir.path().join("test_task");
        fs::create_dir(&task_dir)?;

        // Create a file containing the entire tool_box definition
        let tool_box_file = task_dir.join("tool_box.yml");
        fs::write(
            &tool_box_file,
            r#"
- name: test_group_1
  tools:
    - name: tool_1
      description: "First test tool"
- name: test_group_2
  tools:
    - name: tool_2
      description: "Second test tool"
"#,
        )?;

        // Create task.yml referencing the tool box file
        let task_yaml = format!(
            r#"
system_prompt: "System prompt"
tool_box: $file://{}
"#,
            tool_box_file.display()
        );
        fs::write(task_dir.join("task.yml"), &task_yaml)?;

        let tasklet = Tasklet::from_yaml_file(task_dir.join("task.yml").to_str().unwrap()).await?;

        // Verify the tool_box was interpolated correctly
        let tool_box = tasklet.tool_box.unwrap();
        assert_eq!(tool_box.len(), 2);

        assert_eq!(tool_box[0].name, "test_group_1");
        assert_eq!(tool_box[0].tools[0].name, "tool_1");
        assert_eq!(tool_box[0].tools[0].description, "First test tool");

        assert_eq!(tool_box[1].name, "test_group_2");
        assert_eq!(tool_box[1].tools[0].name, "tool_2");
        assert_eq!(tool_box[1].tools[0].description, "Second test tool");

        Ok(())
    }
}
