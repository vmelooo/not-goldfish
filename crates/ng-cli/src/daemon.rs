//! `ng daemon`: porta de entrada para o binário `ngd` em foreground.

use anyhow::Context;

use crate::i18n::{fill, Msgs};
use crate::util;

pub fn daemon() -> anyhow::Result<()> {
    let m = Msgs::get();
    // ngd is its own binary; exec it so `ng daemon` is just a front door.
    let ngd = util::find_sibling_binary("ngd").with_context(|| m.ngd_not_found)?;
    let status = std::process::Command::new(&ngd)
        .status()
        .with_context(|| fill(m.daemon_running, &[("{path}", &ngd.display())]))?;
    anyhow::ensure!(
        status.success(),
        "{}",
        fill(m.daemon_exited, &[("{status}", &status)])
    );
    Ok(())
}
