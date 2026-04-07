use crate::error::SbResult;
use console::Style;
use tracing::debug;

pub fn execute(quiet: bool, color: bool) -> SbResult<()> {
    debug!("executing version command (quiet={quiet}, color={color})");

    if quiet {
        return Ok(());
    }

    let version_style = if color {
        Style::new().bold()
    } else {
        Style::new()
    };
    let label_style = if color {
        Style::new().dim()
    } else {
        Style::new()
    };

    println!(
        "{}",
        version_style.apply_to(format!("sb {}", env!("CARGO_PKG_VERSION")))
    );
    println!(
        "{} {}",
        label_style.apply_to("commit:"),
        env!("SB_GIT_HASH")
    );
    println!(
        "{} {}",
        label_style.apply_to("built: "),
        env!("SB_BUILD_DATE")
    );
    println!("{} {}", label_style.apply_to("target:"), env!("SB_TARGET"));
    Ok(())
}
