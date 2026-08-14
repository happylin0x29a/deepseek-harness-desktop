// GUI subsystem on Windows for every build: no console window beside the
// app, in debug or release. The shell's own logging goes to the log file
// (%TEMP%\dsh-desktop.log), so the console is not needed for diagnostics.
#![windows_subsystem = "windows"]

fn main() {
    dsh_desktop_lib::run()
}
