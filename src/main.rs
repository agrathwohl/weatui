mod config;

fn main() -> anyhow::Result<()> {
    let cfg = config::Config::load()?;
    println!("{cfg:?}");
    Ok(())
}
