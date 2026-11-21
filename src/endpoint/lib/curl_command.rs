use url::{Host, Url};

/// The largest curl command that the endpoint accepts, in bytes.
pub const MAX_COMMAND_BYTES: usize = 8 * 1024;

/// The largest value of one curl option, in bytes.
const MAX_OPTION_VALUE_BYTES: usize = 4 * 1024;

/// The parts of a curl command that the endpoint needs.
#[derive(Debug, Clone, PartialEq)]
pub struct CurlCommand {
    /// The target URL.
    pub url: Url,
    /// The method from `-X` or `--request`.
    pub method: Option<String>,
    /// The headers from `-H` or `--header`.
    pub headers: Vec<(String, String)>,
    /// The body from `-d`, `--data`, or `--data-raw`.
    pub body: Option<String>,
    /// The credentials from `-u` or `--user`.
    pub user: Option<String>,
    /// True when the command sets `-L` or `--location`.
    pub location: bool,
    /// The limit from `--max-time`, in seconds.
    pub max_time: Option<f64>,
}

impl CurlCommand {
    /// Return the method that the endpoint uses for this command.
    ///
    /// A command with a body and no explicit method uses POST.
    /// A command without a body and no explicit method uses GET.
    pub fn request_method(&self) -> String {
        match &self.method {
            Some(method) => method.clone(),
            None if self.body.is_some() => "POST".to_owned(),
            None => "GET".to_owned(),
        }
    }
}

/// A reason why the parser rejects a curl command.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CurlParseError {
    /// The command exceeds the length limit.
    #[error("curl command exceeds the length limit")]
    CommandTooLong,
    /// The command is empty.
    #[error("curl command is empty")]
    EmptyCommand,
    /// A quote is not closed.
    #[error("curl command has an unmatched quote")]
    InvalidQuoting,
    /// The command does not start with curl.
    #[error("curl command must start with curl")]
    NotCurl,
    /// The command sets an option that the endpoint does not support.
    #[error("unsupported curl option: {0}")]
    UnsupportedOption(String),
    /// An option needs a value.
    #[error("curl option {0} needs a value")]
    MissingValue(&'static str),
    /// An option value exceeds the length limit.
    #[error("curl option value exceeds the length limit")]
    ValueTooLong,
    /// The command sets more than one URL.
    #[error("curl command must have one URL")]
    TooManyUrls,
    /// The command has no URL.
    #[error("curl command needs a URL")]
    MissingUrl,
    /// The URL is not valid.
    #[error("URL is not valid")]
    InvalidUrl,
    /// The URL scheme is not http or https.
    #[error("URL must use http or https")]
    InvalidScheme,
    /// The URL has no host.
    #[error("URL has no host")]
    MissingHost,
    /// A header is not valid.
    #[error("curl header is not valid")]
    InvalidHeader,
    /// A data option reads from a local file.
    #[error("data must not come from a file")]
    DataFromFile,
    /// The method is not a valid token.
    #[error("request method is not valid")]
    InvalidMethod,
    /// The `--max-time` value is not valid.
    #[error("max-time value is not valid")]
    InvalidMaxTime,
}

fn bounded_value(value: &str) -> Result<String, CurlParseError> {
    if value.len() > MAX_OPTION_VALUE_BYTES {
        return Err(CurlParseError::ValueTooLong);
    }
    Ok(value.to_owned())
}

fn take_value(
    inline: Option<&str>,
    args: &mut impl Iterator<Item = String>,
    name: &'static str,
) -> Result<String, CurlParseError> {
    match inline {
        Some(value) => bounded_value(value),
        None => bounded_value(&args.next().ok_or(CurlParseError::MissingValue(name))?),
    }
}

fn option_name(token: &str) -> String {
    token
        .chars()
        .take(48)
        .filter(|ch| ch.is_ascii_alphanumeric() || *ch == '-')
        .collect()
}

fn parse_header(value: &str) -> Result<(String, String), CurlParseError> {
    let (name, header_value) = value.split_once(':').ok_or(CurlParseError::InvalidHeader)?;
    if name.is_empty()
        || name
            .bytes()
            .any(|byte| byte.is_ascii_whitespace() || byte.is_ascii_control())
    {
        return Err(CurlParseError::InvalidHeader);
    }
    let header_value = header_value.trim_start();
    if header_value.bytes().any(|byte| byte == b'\r' || byte == b'\n') {
        return Err(CurlParseError::InvalidHeader);
    }
    Ok((name.to_owned(), header_value.to_owned()))
}

fn parse_method(value: &str) -> Result<String, CurlParseError> {
    if value.is_empty()
        || value.len() > 16
        || !value.bytes().all(|byte| byte.is_ascii_alphabetic())
    {
        return Err(CurlParseError::InvalidMethod);
    }
    Ok(value.to_ascii_uppercase())
}

fn parse_max_time(value: &str) -> Result<f64, CurlParseError> {
    let seconds: f64 = value.parse().map_err(|_| CurlParseError::InvalidMaxTime)?;
    if seconds.is_finite() && seconds >= 0.0 {
        Ok(seconds)
    } else {
        Err(CurlParseError::InvalidMaxTime)
    }
}

fn parse_data(value: String) -> Result<String, CurlParseError> {
    if value.starts_with('@') {
        return Err(CurlParseError::DataFromFile);
    }
    Ok(value)
}

fn set_url(slot: &mut Option<String>, value: String) -> Result<(), CurlParseError> {
    if slot.is_some() {
        return Err(CurlParseError::TooManyUrls);
    }
    *slot = Some(value);
    Ok(())
}

/// Return true when no authority comes after the scheme separator.
fn has_empty_authority(value: &str) -> bool {
    match value.split_once("://") {
        Some((_, rest)) => {
            rest.is_empty() || rest.starts_with('/') || rest.starts_with('?') || rest.starts_with('#')
        }
        None => false,
    }
}

/// Parse a target URL and require an http or https scheme with a host.
pub fn parse_target_url(value: &str) -> Result<Url, CurlParseError> {
    if has_empty_authority(value) {
        return Err(CurlParseError::MissingHost);
    }
    let url = Url::parse(value).map_err(|_| CurlParseError::InvalidUrl)?;
    match url.scheme() {
        "http" | "https" => (),
        _ => return Err(CurlParseError::InvalidScheme),
    }
    if !matches!(
        url.host(),
        Some(Host::Domain(_) | Host::Ipv4(_) | Host::Ipv6(_))
    ) {
        return Err(CurlParseError::MissingHost);
    }
    Ok(url)
}

/// Parse a pasted curl command into the parts that the endpoint needs.
///
/// The parser accepts only the supported options and rejects other options.
/// The parser ignores `-o` and `--output` because the endpoint stores the file.
pub fn parse_curl_command(command: &str) -> Result<CurlCommand, CurlParseError> {
    if command.len() > MAX_COMMAND_BYTES {
        return Err(CurlParseError::CommandTooLong);
    }

    let trimmed = command.trim();
    if trimmed.is_empty() {
        return Err(CurlParseError::EmptyCommand);
    }

    let tokens = shell_words::split(trimmed).map_err(|_| CurlParseError::InvalidQuoting)?;
    let mut args = tokens.into_iter();
    let program = args.next().ok_or(CurlParseError::EmptyCommand)?;
    if program.rsplit('/').next() != Some("curl") {
        return Err(CurlParseError::NotCurl);
    }

    let mut method = None;
    let mut headers = Vec::new();
    let mut body = None;
    let mut user = None;
    let mut location = false;
    let mut max_time = None;
    let mut raw_url = None;
    let mut options_ended = false;

    while let Some(token) = args.next() {
        if options_ended {
            set_url(&mut raw_url, token)?;
            continue;
        }
        if token == "--" {
            options_ended = true;
            continue;
        }
        if let Some(long_option) = token.strip_prefix("--") {
            let (name, inline) = match long_option.split_once('=') {
                Some((name, value)) => (name, Some(value)),
                None => (long_option, None),
            };
            match name {
                "request" => {
                    method = Some(parse_method(&take_value(inline, &mut args, "--request")?)?);
                }
                "header" => {
                    let value = take_value(inline, &mut args, "--header")?;
                    headers.push(parse_header(&value)?);
                }
                "data" | "data-raw" => {
                    let value = take_value(inline, &mut args, "--data")?;
                    body = Some(parse_data(value)?);
                }
                "user" => {
                    user = Some(take_value(inline, &mut args, "--user")?);
                }
                "location" => {
                    if inline.is_some() {
                        return Err(CurlParseError::UnsupportedOption(option_name(name)));
                    }
                    location = true;
                }
                "max-time" => {
                    let value = take_value(inline, &mut args, "--max-time")?;
                    max_time = Some(parse_max_time(&value)?);
                }
                "output" => {
                    let _ = take_value(inline, &mut args, "--output")?;
                }
                _ => return Err(CurlParseError::UnsupportedOption(option_name(name))),
            }
            continue;
        }
        if token.starts_with('-') && token != "-" {
            if token == "-L" {
                location = true;
                continue;
            }
            let short_name = &token[..2];
            if !matches!(short_name, "-X" | "-H" | "-d" | "-u" | "-o") {
                return Err(CurlParseError::UnsupportedOption(option_name(short_name)));
            }
            let attached = (token.len() > 2).then(|| token[2..].to_owned());
            match short_name {
                "-X" => {
                    let value = take_value(attached.as_deref(), &mut args, "-X")?;
                    method = Some(parse_method(&value)?);
                }
                "-H" => {
                    let value = take_value(attached.as_deref(), &mut args, "-H")?;
                    headers.push(parse_header(&value)?);
                }
                "-d" => {
                    let value = take_value(attached.as_deref(), &mut args, "-d")?;
                    body = Some(parse_data(value)?);
                }
                "-u" => {
                    user = Some(take_value(attached.as_deref(), &mut args, "-u")?);
                }
                "-o" => {
                    let _ = take_value(attached.as_deref(), &mut args, "-o")?;
                }
                _ => return Err(CurlParseError::UnsupportedOption(option_name(short_name))),
            }
            continue;
        }
        set_url(&mut raw_url, token)?;
    }

    let raw_url = raw_url.ok_or(CurlParseError::MissingUrl)?;
    let url = parse_target_url(&raw_url)?;

    Ok(CurlCommand {
        url,
        method,
        headers,
        body,
        user,
        location,
        max_time,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_quoted_and_escaped_tokens() {
        let words = shell_words::split(r#"curl -H 'X-A: one two' "https://example.com/a\ b""#)
            .expect("split command");
        assert_eq!(
            words,
            vec!["curl", "-H", "X-A: one two", "https://example.com/a b"]
        );
    }

    #[test]
    fn extracts_supported_options() {
        let command = r#"curl -X PUT -H 'X-One: first' --header "X-Two: second" -d 'payload' -u 'user:pass' -L --max-time 4 -o ignored https://example.com/upload"#;
        let parsed = parse_curl_command(command).expect("parse command");
        assert_eq!(parsed.method.as_deref(), Some("PUT"));
        assert_eq!(
            parsed.headers,
            vec![
                ("X-One".to_owned(), "first".to_owned()),
                ("X-Two".to_owned(), "second".to_owned()),
            ]
        );
        assert_eq!(parsed.body.as_deref(), Some("payload"));
        assert_eq!(parsed.user.as_deref(), Some("user:pass"));
        assert!(parsed.location);
        assert_eq!(parsed.max_time, Some(4.0));
        assert_eq!(parsed.url.as_str(), "https://example.com/upload");
    }

    #[test]
    fn extracts_attached_short_option_values() {
        let command = "curl -XPOST -dhello https://example.com";
        let parsed = parse_curl_command(command).expect("parse command");
        assert_eq!(parsed.method.as_deref(), Some("POST"));
        assert_eq!(parsed.body.as_deref(), Some("hello"));
    }

    #[test]
    fn defaults_to_get_without_a_body() {
        let parsed = parse_curl_command("curl https://example.com/a").expect("parse command");
        assert_eq!(parsed.request_method(), "GET");
    }

    #[test]
    fn defaults_to_post_with_a_body() {
        let parsed = parse_curl_command("curl -d hello https://example.com/a").expect("parse command");
        assert_eq!(parsed.request_method(), "POST");
    }

    #[test]
    fn keeps_an_explicit_method_with_a_body() {
        let parsed =
            parse_curl_command("curl -X PUT -d hello https://example.com/a").expect("parse command");
        assert_eq!(parsed.request_method(), "PUT");
    }

    #[test]
    fn accepts_http_and_https_targets() {
        assert!(parse_target_url("http://example.com/a").is_ok());
        assert!(parse_target_url("https://example.com:8443/a").is_ok());
    }

    #[test]
    fn rejects_other_url_schemes() {
        assert_eq!(
            parse_target_url("file:///etc/passwd"),
            Err(CurlParseError::InvalidScheme)
        );
        assert_eq!(
            parse_target_url("ftp://example.com/a"),
            Err(CurlParseError::InvalidScheme)
        );
    }

    #[test]
    fn rejects_a_url_without_a_host() {
        assert_eq!(
            parse_target_url("http:///path"),
            Err(CurlParseError::InvalidUrl)
        );
    }

    #[test]
    fn rejects_unknown_options() {
        for command in [
            "curl --upload-file x https://example.com",
            "curl -F file=x https://example.com",
            "curl --config /etc/passwd https://example.com",
            "curl -k https://example.com",
        ] {
            assert!(
                matches!(
                    parse_curl_command(command),
                    Err(CurlParseError::UnsupportedOption(_))
                ),
                "command must fail: {command}"
            );
        }
    }

    #[test]
    fn hides_option_values_in_errors() {
        let command = "curl --insecure -H 'Authorization: secret' https://example.com";
        let message = parse_curl_command(command)
            .expect_err("command must fail")
            .to_string();
        assert!(!message.contains("secret"));
        assert!(!message.contains("Authorization"));
    }

    #[test]
    fn rejects_data_from_a_file() {
        assert_eq!(
            parse_curl_command("curl --data @/etc/passwd https://example.com"),
            Err(CurlParseError::DataFromFile)
        );
    }

    #[test]
    fn rejects_a_command_over_the_length_limit() {
        let command = format!("curl https://example.com/{}", "a".repeat(MAX_COMMAND_BYTES));
        assert_eq!(
            parse_curl_command(&command),
            Err(CurlParseError::CommandTooLong)
        );
    }

    #[test]
    fn rejects_an_unmatched_quote() {
        assert_eq!(
            parse_curl_command("curl -H 'unterminated https://example.com"),
            Err(CurlParseError::InvalidQuoting)
        );
    }

    #[test]
    fn rejects_more_than_one_url() {
        assert_eq!(
            parse_curl_command("curl https://example.com https://other.example"),
            Err(CurlParseError::TooManyUrls)
        );
    }

    #[test]
    fn accepts_a_url_after_the_option_separator() {
        let parsed = parse_curl_command("curl -- https://example.com").expect("parse command");
        assert_eq!(parsed.url.as_str(), "https://example.com/");
    }

    #[test]
    fn ignores_the_output_option() {
        let parsed =
            parse_curl_command("curl -o local.txt https://example.com").expect("parse command");
        assert_eq!(parsed.url.as_str(), "https://example.com/");
    }
}
