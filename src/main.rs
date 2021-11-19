use clap::{Arg, App, ArgMatches};

fn main() {
    // argparsing & cli
    let app = App::new("ꟽ ꓤ ꓯ ꓕ ꓱ ꟽ 'metarm'")
        .version("v1.0")
        .about("Metadata removal tool")
        .author("Kris Huber <kris@krishuber.xyz>")
        .arg(
            Arg::with_name("file")
                .long("file")
                .short("f")
                .takes_value(true)
                .help("File you want to remove metadata from")
                .required(true)
        );
    
    let matches: ArgMatches = app.get_matches();
    let filename = matches.value_of("file").unwrap();
    println!("{}", filename);
}
