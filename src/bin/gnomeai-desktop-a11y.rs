#[cfg(target_os = "linux")]
#[path = "../desktop_a11y.rs"]
mod desktop_a11y;

#[cfg(target_os = "linux")]
#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    use anyhow::{Context, bail};

    let mut args = std::env::args().skip(1);
    let action = args.next().unwrap_or_else(|| "inspect".into());
    let result = match action.as_str() {
        "inspect" => {
            let query = args.next().unwrap_or_default();
            let limit = args
                .next()
                .map(|value| value.parse::<usize>().context("limit must be an integer"))
                .transpose()?
                .unwrap_or(140);
            desktop_a11y::inspect(&query, limit).await?
        }
        "activate" => {
            let target = args.next().context("activate requires a semantic target")?;
            let action_name = args.next();
            desktop_a11y::activate(&target, action_name.as_deref()).await?
        }
        "set_text" => {
            let target = args.next().context("set_text requires a semantic target")?;
            let text = args.next().context("set_text requires text")?;
            desktop_a11y::set_text(&target, &text).await?
        }
        "focus" => {
            let target = args.next().context("focus requires a semantic target")?;
            desktop_a11y::focus(&target).await?
        }
        _ => bail!(
            "usage: gnomeai-desktop-a11y inspect [QUERY] [LIMIT] | activate TARGET [ACTION] | set_text TARGET TEXT | focus TARGET"
        ),
    };
    println!("{}", serde_json::to_string(&result)?);
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("semantic desktop navigation is currently available on Linux only");
    std::process::exit(2);
}
