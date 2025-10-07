use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use eyre::Result;

#[derive(Debug, PartialEq)]
pub enum InputMode {
    Normal,
    AddNode,
    SetAlias,
    ConfirmRemove,
}

#[derive(Debug)]
pub struct InputHandler {
    pub mode: InputMode,
    pub input_buffer: String,
    pub cursor_position: usize,
    pub node_to_remove: Option<String>,
    pub node_to_alias: Option<String>,
}

impl InputHandler {
    pub fn new() -> Self {
        Self {
            mode: InputMode::Normal,
            input_buffer: String::new(),
            cursor_position: 0,
            node_to_remove: None,
            node_to_alias: None,
        }
    }

    pub fn handle_input(&mut self, key: KeyEvent) -> Result<Option<InputResult>> {
        match self.mode {
            InputMode::Normal => Ok(None),
            InputMode::AddNode => self.handle_add_node_input(key),
            InputMode::SetAlias => self.handle_set_alias_input(key),
            InputMode::ConfirmRemove => self.handle_confirm_remove_input(key),
        }
    }

    fn handle_add_node_input(&mut self, key: KeyEvent) -> Result<Option<InputResult>> {
        match key.code {
            KeyCode::Enter => {
                let url = self.input_buffer.trim().to_string();
                self.clear_input();
                self.mode = InputMode::Normal;

                if !url.is_empty() {
                    Ok(Some(InputResult::AddNode(url)))
                } else {
                    Ok(None)
                }
            }
            KeyCode::Esc => {
                self.clear_input();
                self.mode = InputMode::Normal;
                Ok(None)
            }
            KeyCode::Backspace => {
                if self.cursor_position > 0 {
                    self.input_buffer.remove(self.cursor_position - 1);
                    self.cursor_position -= 1;
                }
                Ok(None)
            }
            KeyCode::Delete => {
                if self.cursor_position < self.input_buffer.len() {
                    self.input_buffer.remove(self.cursor_position);
                }
                Ok(None)
            }
            KeyCode::Left => {
                if self.cursor_position > 0 {
                    self.cursor_position -= 1;
                }
                Ok(None)
            }
            KeyCode::Right => {
                if self.cursor_position < self.input_buffer.len() {
                    self.cursor_position += 1;
                }
                Ok(None)
            }
            KeyCode::Home => {
                self.cursor_position = 0;
                Ok(None)
            }
            KeyCode::End => {
                self.cursor_position = self.input_buffer.len();
                Ok(None)
            }
            KeyCode::Char(c) => {
                if key.modifiers.contains(KeyModifiers::CONTROL) {
                    match c {
                        'c' => {
                            self.clear_input();
                            self.mode = InputMode::Normal;
                        }
                        'u' => {
                            self.input_buffer.clear();
                            self.cursor_position = 0;
                        }
                        _ => {}
                    }
                } else {
                    self.input_buffer.insert(self.cursor_position, c);
                    self.cursor_position += 1;
                }
                Ok(None)
            }
            _ => Ok(None),
        }
    }

    fn handle_set_alias_input(&mut self, key: KeyEvent) -> Result<Option<InputResult>> {
        match key.code {
            KeyCode::Enter => {
                let alias = self.input_buffer.trim().to_string();
                let node_url = self.node_to_alias.take();
                self.clear_input();
                self.mode = InputMode::Normal;

                if let Some(url) = node_url {
                    let alias_option = if alias.is_empty() { None } else { Some(alias) };
                    Ok(Some(InputResult::SetNodeAlias {
                        url,
                        alias: alias_option,
                    }))
                } else {
                    Ok(None)
                }
            }
            KeyCode::Esc => {
                self.clear_input();
                self.node_to_alias = None;
                self.mode = InputMode::Normal;
                Ok(None)
            }
            KeyCode::Backspace => {
                if self.cursor_position > 0 {
                    self.input_buffer.remove(self.cursor_position - 1);
                    self.cursor_position -= 1;
                }
                Ok(None)
            }
            KeyCode::Delete => {
                if self.cursor_position < self.input_buffer.len() {
                    self.input_buffer.remove(self.cursor_position);
                }
                Ok(None)
            }
            KeyCode::Left => {
                if self.cursor_position > 0 {
                    self.cursor_position -= 1;
                }
                Ok(None)
            }
            KeyCode::Right => {
                if self.cursor_position < self.input_buffer.len() {
                    self.cursor_position += 1;
                }
                Ok(None)
            }
            KeyCode::Home => {
                self.cursor_position = 0;
                Ok(None)
            }
            KeyCode::End => {
                self.cursor_position = self.input_buffer.len();
                Ok(None)
            }
            KeyCode::Char(c) => {
                if key.modifiers.contains(KeyModifiers::CONTROL) {
                    match c {
                        'c' => {
                            self.clear_input();
                            self.node_to_alias = None;
                            self.mode = InputMode::Normal;
                        }
                        'u' => {
                            self.input_buffer.clear();
                            self.cursor_position = 0;
                        }
                        _ => {}
                    }
                } else {
                    self.input_buffer.insert(self.cursor_position, c);
                    self.cursor_position += 1;
                }
                Ok(None)
            }
            _ => Ok(None),
        }
    }

    fn handle_confirm_remove_input(&mut self, key: KeyEvent) -> Result<Option<InputResult>> {
        match key.code {
            KeyCode::Char('y' | 'Y') => {
                if let Some(url) = self.node_to_remove.take() {
                    self.mode = InputMode::Normal;
                    Ok(Some(InputResult::RemoveNode(url)))
                } else {
                    self.mode = InputMode::Normal;
                    Ok(None)
                }
            }
            KeyCode::Char('n' | 'N') | KeyCode::Esc => {
                self.node_to_remove = None;
                self.mode = InputMode::Normal;
                Ok(None)
            }
            _ => Ok(None),
        }
    }

    pub fn start_add_node(&mut self) {
        self.mode = InputMode::AddNode;
        self.input_buffer = "http://".to_string();
        self.cursor_position = self.input_buffer.len();
    }

    pub fn start_set_alias(&mut self, node_url: String, current_alias: Option<String>) {
        self.mode = InputMode::SetAlias;
        self.node_to_alias = Some(node_url);
        self.input_buffer = current_alias.unwrap_or_default();
        self.cursor_position = self.input_buffer.len();
    }

    pub fn start_remove_node(&mut self, node_url: String) {
        self.mode = InputMode::ConfirmRemove;
        self.node_to_remove = Some(node_url);
    }

    fn clear_input(&mut self) {
        self.input_buffer.clear();
        self.cursor_position = 0;
    }

    pub fn get_prompt(&self) -> Option<(&str, &str, usize)> {
        match self.mode {
            InputMode::Normal => None,
            InputMode::AddNode => Some(("Add Node URL", &self.input_buffer, self.cursor_position)),
            InputMode::SetAlias => Some((
                "Set Node Alias (empty to clear)",
                &self.input_buffer,
                self.cursor_position,
            )),
            InputMode::ConfirmRemove => None,
        }
    }

    pub fn get_confirmation_message(&self) -> Option<String> {
        match self.mode {
            InputMode::ConfirmRemove => self
                .node_to_remove
                .as_ref()
                .map(|url| format!("Remove node '{url}'? (y/n)")),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub enum InputResult {
    AddNode(String),
    SetNodeAlias { url: String, alias: Option<String> },
    RemoveNode(String),
}

impl Default for InputHandler {
    fn default() -> Self {
        Self::new()
    }
}
