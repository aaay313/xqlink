use std::process::Child;
use std::process::Command;
use std::process::Stdio;

/// 引擎进程的两条管道：父进程侧的写端(stdin)与读端(stdout)。
pub type Pipes = (Box<dyn std::io::Write>, Box<dyn std::io::BufRead>);

/// 启动引擎子进程，返回 (子进程, (stdin写端, stdout读端))。
///
/// 注意：Windows 分支**不使用** `Stdio::piped()`。
///
/// 原因：本机（`x86_64-pc-windows-gnu` 目标）Rust 标准库的匿名管道实现会稳定失败：
///   `Os { code: 231, kind: Uncategorized, message: "所有的管道范例都在使用中。" }`
/// 即 `ERROR_PIPE_BUSY`。该现象与子进程无关（连 `spawn("cmd.exe")` 也一样失败），
/// 与链接方式也无关（`link-self-contained` 开/关、gcc 链接或 rust-lld 链接都复现）；
/// 而直接调用 kernel32 的 `CreatePipe` 则完全正常。
/// 因此这里手工建管道，并用 `Stdio::from_raw_handle` 交给子进程。
#[cfg(target_os = "windows")]
pub fn new(libs: &std::path::Path) -> (Child, Pipes) {
    use std::ffi::c_void;
    use std::io::BufReader;
    use std::os::windows::io::{FromRawHandle, RawHandle};
    use std::os::windows::process::CommandExt;

    const HANDLE_FLAG_INHERIT: u32 = 1;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    unsafe extern "system" {
        fn CreatePipe(
            ph_read_pipe: *mut *mut c_void,
            ph_write_pipe: *mut *mut c_void,
            lp_pipe_attributes: *mut c_void,
            n_size: u32,
        ) -> i32;
        fn SetHandleInformation(h_object: *mut c_void, dw_mask: u32, dw_flags: u32) -> i32;
        fn CloseHandle(h_object: *mut c_void) -> i32;
    }

    /// 建一条匿名管道，并把子进程要继承的那一端标记为可继承。
    unsafe fn pipe(child_end_is_read: bool) -> (*mut c_void, *mut c_void) {
        let mut a: *mut c_void = std::ptr::null_mut();
        let mut b: *mut c_void = std::ptr::null_mut();
        if unsafe { CreatePipe(&mut a, &mut b, std::ptr::null_mut(), 0) } == 0 {
            panic!("CreatePipe failed: {}", std::io::Error::last_os_error());
        }
        // a 是读端, b 是写端
        let child_end = if child_end_is_read { a } else { b };
        if unsafe { SetHandleInformation(child_end, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) } == 0 {
            panic!("SetHandleInformation failed: {}", std::io::Error::last_os_error());
        }
        (a, b)
    }

    unsafe {
        // 子进程的 stdin：子进程持读端，父进程持写端
        let (child_stdin_read, parent_stdin_write) = pipe(true);
        // 子进程的 stdout：子进程持写端，父进程持读端
        let (parent_stdout_read, child_stdout_write) = pipe(false);

        let child = Command::new(libs.join("pikafish-windows.exe"))
            .stdin(Stdio::from_raw_handle(child_stdin_read as RawHandle))
            .stdout(Stdio::from_raw_handle(child_stdout_write as RawHandle))
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
            .expect("Unable to run engine");

        // 子进程已继承，父进程必须关掉自己这一份子端句柄，
        // 否则父进程读 stdout 时永远等不到 EOF。
        CloseHandle(child_stdin_read);
        CloseHandle(child_stdout_write);

        let stdin = Box::new(std::fs::File::from_raw_handle(parent_stdin_write as RawHandle))
            as Box<dyn std::io::Write>;
        let stdout = Box::new(BufReader::new(std::fs::File::from_raw_handle(
            parent_stdout_read as RawHandle,
        ))) as Box<dyn std::io::BufRead>;

        (child, (stdin, stdout))
    }
}

#[cfg(target_os = "macos")]
pub fn new(libs: &std::path::Path) -> (Child, Pipes) {
    let mut child = Command::new(libs.join("pikafish-macos"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("Unable to run engine");
    let stdin = Box::new(child.stdin.take().unwrap()) as Box<dyn std::io::Write>;
    let stdout =
        Box::new(std::io::BufReader::new(child.stdout.take().unwrap())) as Box<dyn std::io::BufRead>;
    (child, (stdin, stdout))
}

#[cfg(target_os = "linux")]
pub fn new(libs: &std::path::Path) -> (Child, Pipes) {
    let mut child = Command::new(libs.join("pikafish-linux"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("Unable to run engine");
    let stdin = Box::new(child.stdin.take().unwrap()) as Box<dyn std::io::Write>;
    let stdout =
        Box::new(std::io::BufReader::new(child.stdout.take().unwrap())) as Box<dyn std::io::BufRead>;
    (child, (stdin, stdout))
}
