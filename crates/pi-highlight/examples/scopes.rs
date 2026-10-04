//! Print the highlight spans for a snippet.
//!
//! Usage: cargo run -p pi-highlight --example scopes -- <lang> <code>

fn main() {
    let mut args = std::env::args().skip(1);
    let language = args.next().unwrap_or_else(|| "rs".to_string());
    let code = args.next().unwrap_or_else(|| "fn main() {}".to_string());
    match pi_highlight::highlight(&code, &language) {
        Some(lines) => {
            for (index, line) in lines.iter().enumerate() {
                for span in line {
                    println!("{index}: {:>12}  {:?}", span.scope.theme_key(), span.text);
                }
            }
        }
        None => println!("no grammar for {language}"),
    }
}
