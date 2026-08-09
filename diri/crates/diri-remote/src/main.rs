fn main() {
    let executable = match std::env::current_exe() {
        Ok(path) => path,
        Err(error) => {
            eprintln!("diri-remote: cannot resolve current executable: {error}");
            std::process::exit(diri_remote::EXIT_FAILURE);
        }
    };
    let mut stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    let mut stderr = std::io::stderr();
    let code = diri_remote::execute(
        std::env::args().skip(1),
        &executable,
        &mut stdin,
        &mut stdout,
        &mut stderr,
    );
    if code != diri_remote::EXIT_OK {
        std::process::exit(code);
    }
}
