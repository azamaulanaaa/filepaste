use clap::Parser;

#[derive(Parser, Debug)]
#[command(author, version)]
pub struct Args {
    #[arg(short, long)]
    pub config: Option<String>,

    #[arg(short, long, action = clap::ArgAction::Count)]
    pub verbose: u8,
}
