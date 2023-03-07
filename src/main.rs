use anyhow::{anyhow, bail, Result};
use clap::Parser;
use console::style;
use futures::stream::StreamExt;
use log::{debug, trace};
use reqwest::header::{HeaderMap, AUTHORIZATION};
use reqwest::{Client, RequestBuilder};
use reqwest_eventsource::{Event, EventSource};
use rustyline::error::ReadlineError;
use rustyline::DefaultEditor;
use std::io::Write;

mod model;

use model::*;

/// Command-line options
#[derive(Parser, Debug)]
#[command(about, long_about = None, trailing_var_arg=true)]
struct Options {
    /// Whether to use streaming API
    #[arg(long)]
    pub no_stream: bool,

    /// The model to query
    #[arg(long, default_value_t = String::from("gpt-3.5-turbo"))]
    pub model: String,

    /// Sampling temperature to use, between 0 and 2.
    #[arg(
        long,
        hide_short_help = true,
        long_help = r#"Higher values like 0.8 will make the output more random, while lower values like 0.2 will make it more focused and deterministic.
We generally recommend altering this or top_p but not both."#
    )]
    pub temperature: Option<f64>,

    /// Probability of nucleus sampling
    #[arg(
        long,
        hide_short_help = true,
        long_help = r#"An alternative to sampling with temperature, called nucleus sampling, where the model considers the results of the tokens with top_p probability mass. So 0.1 means only the tokens comprising the top 10% probability mass are considered.
We generally recommend altering this or temperature but not both."#
    )]
    pub top_p: Option<f64>,

    /// The prompt to ask. Leave it empty to activate interactive mode
    pub prompt: Vec<String>,
}

const READLINE_HISTORY: &str = ".heygpt_history";

const OPENAI_API_KEY: &str = "OPENAI_API_KEY";
const OPENAI_API_BASE: &str = "OPENAI_API_BASE";

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    env_logger::init();

    let options = Options::parse();

    // get OPENAI_API_KEY from environment variable
    let api_key =
        std::env::var(OPENAI_API_KEY).map_err(|_| anyhow!("{} not set", OPENAI_API_KEY))?;

    let api_base = std::env::var(OPENAI_API_BASE).unwrap_or("https://api.openai.com/v1".into());

    // Enter interactive mode if prompt is empty
    let interactive = options.prompt.is_empty();

    let mut session = Session::new(options, api_key, api_base);
    if !interactive {
        session.run_one_shot().await?;
    } else {
        session.run_interactive().await?;
    }

    Ok(())
}

struct Session {
    /// Command-line options
    options: Options,

    /// OpenAI API key
    api_key: String,

    /// OpenAI API base URL
    api_base: String,

    /// Messages history
    messages: Vec<Message>,
}

impl Session {
    pub fn new(options: Options, api_key: String, api_base: String) -> Self {
        Self {
            options,
            api_key,
            api_base,
            messages: Vec::new(),
        }
    }

    pub async fn run_one_shot(&mut self) -> Result<()> {
        let prompt = self.options.prompt.join(" ");

        self.messages.push(Message {
            role: "user".to_string(),
            content: prompt,
        });

        let _ = self.complete_and_print().await?;
        Ok(())
    }

    pub async fn run_interactive(&mut self) -> Result<()> {
        let mut rl = DefaultEditor::new()?;

        // Persist input history in `$HOME/.heygpt_history`
        let history_file = {
            let mut p = dirs::home_dir().unwrap();
            p.push(READLINE_HISTORY);
            p.to_str().unwrap().to_owned()
        };
        let _ = rl.load_history(&history_file);

        loop {
            let readline = rl.readline(&format!("{} => ", style("user").bold().cyan()));
            let prompt = match readline {
                Ok(line) => {
                    if line.is_empty() {
                        continue; // ignore empty input
                    }
                    rl.add_history_entry(line.as_str())?;
                    line
                }
                Err(ReadlineError::Interrupted) => {
                    println!("CTRL-C");
                    break;
                }
                Err(ReadlineError::Eof) => {
                    println!("CTRL-D");
                    break;
                }
                Err(err) => {
                    bail!("Readline error: {:?}", err);
                }
            };

            self.messages.push(Message {
                role: "user".to_string(),
                content: prompt,
            });

            print!("{} => ", style("assistant").bold().green());
            std::io::stdout().flush()?;

            let response = self.complete_and_print().await?;

            self.messages.push(response);
        }

        rl.append_history(&history_file)?;
        Ok(())
    }

    /// Complete the message sequence and returns the next message.
    /// Meanwhile, output the response to stdout.
    async fn complete_and_print(&self) -> Result<Message> {
        // Build the request
        let data = Request {
            model: self.options.model.clone(),
            stream: !self.options.no_stream,
            messages: self.messages.to_vec(),
            temperature: self.options.temperature,
            top_p: self.options.top_p,
        };

        let mut headers = HeaderMap::new();
        headers.insert(
            AUTHORIZATION,
            format!("Bearer {}", self.api_key).parse().unwrap(),
        );

        let client = Client::new();
        let req = client
            .post(format!("{}/chat/completions", &self.api_base))
            .headers(headers)
            .json(&data);

        debug!("Request body: {:?}", &data);

        if !self.options.no_stream {
            self.do_stream_request(req).await
        } else {
            self.do_non_stream_request(req).await
        }
    }

    async fn do_stream_request(&self, req: RequestBuilder) -> Result<Message> {
        let mut full_message = Message::default();

        let mut es = EventSource::new(req)?;
        while let Some(event) = es.next().await {
            match event {
                Ok(Event::Open) => {
                    debug!("response stream opened")
                }
                Ok(Event::Message(message)) if message.data == "[DONE]" => {
                    debug!("response stream ended with [DONE]");
                    println!();
                    break;
                }
                Ok(Event::Message(message)) => {
                    trace!("response stream message: {:?}", &message);
                    let message: ResponseStreamMessage = serde_json::from_str(&message.data)?;
                    let delta = message.choices.into_iter().next().unwrap().delta;
                    if let Some(role) = delta.role {
                        full_message.role.push_str(&role);
                    }
                    if let Some(mut content) = delta.content {
                        // Trick: Sometimes the response starts with a newline. Strip it here.
                        if content.starts_with("\n") && full_message.content.is_empty() {
                            content = content.trim_start().to_owned();
                        }
                        print!("{}", content);
                        full_message.content.push_str(&content);
                    }
                    std::io::stdout().flush().unwrap();
                }
                Err(err) => {
                    es.close();
                    bail!("EventSource stream error: {}", err);
                }
            }
        }

        debug!("response stream full message: {:?}", &full_message);

        Ok(full_message)
    }

    async fn do_non_stream_request(&self, req: RequestBuilder) -> Result<Message> {
        let response: ResponseMessage = req.send().await?.json().await?;

        debug!("response message: {:?}", &response);

        let mut message = response.choices[0].message.clone();

        // Trick: Sometimes the response starts with a newline. Strip it here.
        if message.content.starts_with("\n") {
            message.content = message.content.trim_start().to_owned();
        }

        println!("{}", &message.content);

        Ok(message)
    }
}
