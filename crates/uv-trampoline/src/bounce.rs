#![allow(clippy::disallowed_types)]
use std::ffi::{CString, c_void};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::mem::size_of;
use std::path::{Path, PathBuf};
use std::vec::Vec;

use windows::Win32::Foundation::{LPARAM, WPARAM};
use windows::Win32::{
    Foundation::{
        CloseHandle, HANDLE, HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE, SetHandleInformation, TRUE,
    },
    System::Console::{
        GetStdHandle, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, SetConsoleCtrlHandler, SetStdHandle,
    },
    System::Environment::GetCommandLineA,
    System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectA, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JobObjectExtendedLimitInformation, QueryInformationJobObject, SetInformationJobObject,
    },
    System::Threading::{
        CreateProcessA, GetExitCodeProcess, GetStartupInfoA, INFINITE, PROCESS_CREATION_FLAGS,
        PROCESS_INFORMATION, STARTF_USESTDHANDLES, STARTUPINFOA, WaitForInputIdle,
        WaitForSingleObject,
    },
    UI::WindowsAndMessaging::{
        CreateWindowExA, DestroyWindow, GetMessageA, HWND_MESSAGE, MSG, PEEK_MESSAGE_REMOVE_TYPE,
        PeekMessageA, PostMessageA, WINDOW_EX_STYLE, WINDOW_STYLE,
    },
};
use windows::core::{BOOL, PSTR, s};

use crate::{error, format, warn};

const PATH_LEN_SIZE: usize = size_of::<u32>();
const MAX_PATH_LEN: u32 = 32 * 1024;

/// The kind of trampoline.
enum TrampolineKind {
    /// The trampoline should execute itself, it's a zipped Python script.
    Script,
    /// The trampoline should just execute Python, it's a proxy Python executable.
    Python,
}

impl TrampolineKind {
    const fn magic_number(&self) -> &'static [u8; 4] {
        match self {
            Self::Script => b"UVSC",
            Self::Python => b"UVPY",
        }
    }

    fn from_buffer(buffer: &[u8]) -> Option<Self> {
        if buffer.ends_with(Self::Script.magic_number()) {
            Some(Self::Script)
        } else if buffer.ends_with(Self::Python.magic_number()) {
            Some(Self::Python)
        } else {
            None
        }
    }
}

/// Transform `<command> <arguments>` to `python <command> <arguments>` or `python <arguments>`
/// depending on the [`TrampolineKind`].
fn make_child_cmdline() -> CString {
    let executable_name = std::env::current_exe().unwrap_or_else(|_| {
        error_and_exit("Failed to get executable name");
    });
    let (kind, python_exe) = read_trampoline_metadata(executable_name.as_ref());
    let mut child_cmdline = Vec::<u8>::new();

    push_quoted_path(python_exe.as_ref(), &mut child_cmdline);
    child_cmdline.push(b' ');

    // Only execute the trampoline again if it's a script, otherwise, just invoke Python.
    match kind {
        TrampolineKind::Python => {
            // SAFETY: `std::env::set_var` is safe to call on Windows, and
            // this code only ever runs on Windows.
            unsafe {
                // Setting this env var will cause `getpath.py` to set
                // `executable` to the path to this trampoline. This is
                // the approach taken by CPython for Python Launchers
                // (in `launcher.c`). This allows virtual environments to
                // be correctly detected when using trampolines.
                std::env::set_var("__PYVENV_LAUNCHER__", &executable_name);

                // If this is not a virtual environment and `PYTHONHOME` has
                // not been set, then set `PYTHONHOME` to the parent directory of
                // the executable. This ensures that the correct installation
                // directories are added to `sys.path` when running with a junction
                // trampoline.
                let python_home_set =
                    std::env::var("PYTHONHOME").is_ok_and(|home| !home.is_empty());
                if !is_virtualenv(python_exe.as_path()) && !python_home_set {
                    std::env::set_var(
                        "PYTHONHOME",
                        python_exe
                            .parent()
                            .expect("Python executable should have a parent directory"),
                    );
                }
            }
        }
        TrampolineKind::Script => {
            // Use the full executable name because CMD only passes the name of the executable (but not the path)
            // when e.g. invoking `black` instead of `<PATH_TO_VENV>/Scripts/black` and Python then fails
            // to find the file. Unfortunately, this complicates things because we now need to split the executable
            // from the arguments string...
            push_quoted_path(executable_name.as_ref(), &mut child_cmdline);
        }
    }

    push_arguments(&mut child_cmdline);

    child_cmdline.push(b'\0');

    // Helpful when debugging trampoline issues
    // warn!(
    //     "executable_name: '{}'\nnew_cmdline: {}",
    //     &*executable_name.to_string_lossy(),
    //     std::str::from_utf8(child_cmdline.as_slice()).unwrap()
    // );

    CString::from_vec_with_nul(child_cmdline).unwrap_or_else(|_| {
        error_and_exit("Child command line is not correctly null terminated");
    })
}

fn push_quoted_path(path: &Path, command: &mut Vec<u8>) {
    command.push(b'"');
    for byte in path.as_os_str().as_encoded_bytes() {
        if *byte == b'"' {
            // 3 double quotes: one to end the quoted span, one to become a literal double-quote,
            // and one to start a new quoted span.
            command.extend(br#"""""#);
        } else {
            command.push(*byte);
        }
    }
    command.extend(br#"""#);
}

/// Checks if the given executable is part of a virtual environment
///
/// Checks if a `pyvenv.cfg` file exists in grandparent directory of the given executable.
/// PEP 405 specifies a more robust procedure (checking both the parent and grandparent
/// directory and then scanning for a `home` key), but in practice we have found this to
/// be unnecessary.
fn is_virtualenv(executable: &Path) -> bool {
    executable
        .parent()
        .and_then(Path::parent)
        .map(|path| path.join("pyvenv.cfg").is_file())
        .unwrap_or(false)
}

/// Reads the executable binary from the back to find:
///
/// * The path to the Python executable
/// * The kind of trampoline we are executing
///
/// The executable is expected to have the following format:
///
/// * The file must end with the magic number 'UVPY' or 'UVSC' (identifying the trampoline kind)
/// * The last 4 bytes (little endian) are the length of the path to the Python executable.
/// * The path encoded as UTF-8 comes right before the length
///
/// # Panics
///
/// If there's any IO error, or the file does not conform to the specified format.
fn read_trampoline_metadata(executable_name: &Path) -> (TrampolineKind, PathBuf) {
    let mut file_handle = File::open(executable_name).unwrap_or_else(|_| {
        print_last_error_and_exit(&format!(
            "Failed to open executable '{}'",
            &*executable_name.to_string_lossy(),
        ));
    });

    let metadata = executable_name.metadata().unwrap_or_else(|_| {
        print_last_error_and_exit(&format!(
            "Failed to get the size of the executable '{}'",
            &*executable_name.to_string_lossy(),
        ));
    });
    let file_size = metadata.len();

    // Start with a size of 1024 bytes which should be enough for most paths but avoids reading the
    // entire file.
    let mut buffer: Vec<u8> = Vec::new();
    let mut bytes_to_read = 1024.min(u32::try_from(file_size).unwrap_or(u32::MAX));

    let mut kind;
    let path: String = loop {
        // SAFETY: Casting to usize is safe because we only support 64bit systems where usize is guaranteed to be larger than u32.
        buffer.resize(bytes_to_read as usize, 0);

        file_handle
            .seek(SeekFrom::Start(file_size - u64::from(bytes_to_read)))
            .unwrap_or_else(|_| {
                print_last_error_and_exit("Failed to set the file pointer to the end of the file");
            });

        // Pulls in core::fmt::{write, Write, getcount}
        let read_bytes = file_handle.read(&mut buffer).unwrap_or_else(|_| {
            print_last_error_and_exit("Failed to read the executable file");
        });

        // Truncate the buffer to the actual number of bytes read.
        buffer.truncate(read_bytes);

        let Some(inner_kind) = TrampolineKind::from_buffer(&buffer) else {
            error_and_exit(
                "Magic number 'UVSC' or 'UVPY' not found at the end of the file. Did you append the magic number, the length and the path to the python executable at the end of the file?",
            );
        };
        kind = inner_kind;

        // Remove the magic number
        buffer.truncate(buffer.len() - kind.magic_number().len());

        let path_len = match buffer.get(buffer.len() - PATH_LEN_SIZE..) {
            Some(path_len) => {
                let path_len = u32::from_le_bytes(path_len.try_into().unwrap_or_else(|_| {
                    error_and_exit("Slice length is not equal to 4 bytes");
                }));

                if path_len > MAX_PATH_LEN {
                    error_and_exit(&format!(
                        "Only paths with a length up to 32KBs are supported but the python path has a length of {}",
                        path_len
                    ));
                }

                // SAFETY: path len is guaranteed to be less than 32KBs
                path_len as usize
            }
            None => {
                error_and_exit(
                    "Python executable length missing. Did you write the length of the path to the Python executable before the Magic number?",
                );
            }
        };

        // Remove the path length
        buffer.truncate(buffer.len() - PATH_LEN_SIZE);

        if let Some(path_offset) = buffer.len().checked_sub(path_len) {
            buffer.drain(..path_offset);

            break String::from_utf8(buffer).unwrap_or_else(|_| {
                error_and_exit("Python executable path is not a valid UTF-8 encoded path");
            });
        } else {
            // SAFETY: Casting to u32 is safe because `path_len` is guaranteed to be less than 32KBs,
            // MAGIC_NUMBER is 4 bytes and PATH_LEN_SIZE is 4 bytes.
            bytes_to_read = (path_len + kind.magic_number().len() + PATH_LEN_SIZE) as u32;

            if u64::from(bytes_to_read) > file_size {
                error_and_exit(
                    "The length of the python executable path exceeds the file size. Verify that the path length is appended to the end of the launcher script as a u32 in little endian",
                );
            }
        }
    };

    let path = PathBuf::from(path);
    let path = if path.is_absolute() {
        path
    } else {
        let parent_dir = match executable_name.parent() {
            Some(parent) => parent,
            None => {
                error_and_exit("Executable path has no parent directory");
            }
        };
        parent_dir.join(path)
    };

    let path = if !path.is_absolute() || matches!(kind, TrampolineKind::Script) {
        // NOTICE: dunce adds 5kb~
        // TODO(john): In order to avoid resolving junctions and symlinks for relative paths and
        // scripts, we can consider reverting https://github.com/astral-sh/uv/pull/5750/files#diff-969979506be03e89476feade2edebb4689a9c261f325988d3c7efc5e51de26d1L273-L277.
        dunce::canonicalize(path.as_path()).unwrap_or_else(|_| {
            error_and_exit("Failed to canonicalize script path");
        })
    } else {
        // For Python trampolines with absolute paths, we skip `dunce::canonicalize` to
        // avoid resolving junctions.
        path
    };

    (kind, path)
}

fn push_arguments(output: &mut Vec<u8>) {
    // SAFETY: We rely on `GetCommandLineA` to return a valid pointer to a null terminated string.
    let arguments_as_str = unsafe { GetCommandLineA() };
    let arguments_as_bytes = unsafe { arguments_as_str.as_bytes() };

    // Skip over the executable name and then push the rest of the arguments
    let after_executable = skip_one_argument(arguments_as_bytes);

    output.extend_from_slice(after_executable)
}

fn skip_one_argument(arguments: &[u8]) -> &[u8] {
    let mut quoted = false;
    let mut offset = 0;
    let mut bytes_iter = arguments.iter().peekable();

    // Implements https://learn.microsoft.com/en-us/cpp/c-language/parsing-c-command-line-arguments?view=msvc-170
    while let Some(byte) = bytes_iter.next().copied() {
        match byte {
            b'"' => {
                quoted = !quoted;
            }
            b'\\' => {
                // Skip over escaped quotes or even number of backslashes.
                if matches!(bytes_iter.peek().copied(), Some(&b'\"' | &b'\\')) {
                    offset += 1;
                    bytes_iter.next();
                }
            }
            byte => {
                if byte.is_ascii_whitespace() && !quoted {
                    break;
                }
            }
        }

        offset += 1;
    }

    &arguments[offset..]
}

fn make_job_object() -> HANDLE {
    let job = unsafe { CreateJobObjectA(None, None) }
        .unwrap_or_else(|_| print_last_error_and_exit("Job creation failed"));
    let mut job_info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    let mut retlen = 0u32;
    if unsafe {
        QueryInformationJobObject(
            Some(job),
            JobObjectExtendedLimitInformation,
            &mut job_info as *mut _ as *mut c_void,
            size_of_val(&job_info) as u32,
            Some(&mut retlen),
        )
    }
    .is_err()
    {
        print_last_error_and_exit("Job information querying failed");
    }
    job_info.BasicLimitInformation.LimitFlags |= JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    job_info.BasicLimitInformation.LimitFlags |= JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK;
    if unsafe {
        SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &job_info as *const _ as *const c_void,
            size_of_val(&job_info) as u32,
        )
    }
    .is_err()
    {
        print_last_error_and_exit("Job information setting failed");
    }
    job
}

fn spawn_child(si: &STARTUPINFOA, child_cmdline: CString) -> HANDLE {
    // See distlib/PC/launcher.c::run_child
    if (si.dwFlags & STARTF_USESTDHANDLES).0 != 0 {
        // ignore errors, if the handles are not inheritable/valid, then nothing we can do
        unsafe { SetHandleInformation(si.hStdInput, HANDLE_FLAG_INHERIT.0, HANDLE_FLAG_INHERIT) }
            .unwrap_or_else(|_| warn!("Making stdin inheritable failed"));
        unsafe { SetHandleInformation(si.hStdOutput, HANDLE_FLAG_INHERIT.0, HANDLE_FLAG_INHERIT) }
            .unwrap_or_else(|_| warn!("Making stdout inheritable failed"));
        unsafe { SetHandleInformation(si.hStdError, HANDLE_FLAG_INHERIT.0, HANDLE_FLAG_INHERIT) }
            .unwrap_or_else(|_| warn!("Making stderr inheritable failed"));
    }
    let mut child_process_info = PROCESS_INFORMATION::default();
    unsafe {
        CreateProcessA(
            None,
            // Why does this have to be mutable? Who knows. But it's not a mistake --
            // MS explicitly documents that this buffer might be mutated by CreateProcess.
            Some(PSTR::from_raw(child_cmdline.as_ptr() as *mut _)),
            None,
            None,
            true,
            PROCESS_CREATION_FLAGS(0),
            None,
            None,
            si,
            &mut child_process_info,
        )
    }
    .unwrap_or_else(|_| {
        print_last_error_and_exit("Failed to spawn the python child process");
    });
    unsafe { CloseHandle(child_process_info.hThread) }.unwrap_or_else(|_| {
        print_last_error_and_exit("Failed to close handle to python child process main thread");
    });
    // Return handle to child process.
    child_process_info.hProcess
}

// Apparently, the Windows C runtime has a secret way to pass file descriptors into child
// processes, by using the .lpReserved2 field. We want to close those file descriptors too.
// The UCRT source code has details on the memory layout (see also initialize_inherited_file_handles_nolock):
// https://github.com/huangqinjin/ucrt/blob/10.0.19041.0/lowio/ioinit.cpp#L190-L223
fn close_handles(si: &STARTUPINFOA) {
    // See distlib/PC/launcher.c::cleanup_standard_io()
    // Unlike cleanup_standard_io(), we don't close STD_ERROR_HANDLE to retain warn!
    for std_handle in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE] {
        if let Ok(handle) = unsafe { GetStdHandle(std_handle) } {
            unsafe { CloseHandle(handle) }.unwrap_or_else(|_| {
                warn!("Failed to close standard device handle {}", handle.0 as u32);
            });
            unsafe { SetStdHandle(std_handle, INVALID_HANDLE_VALUE) }.unwrap_or_else(|_| {
                warn!("Failed to modify standard device handle {}", std_handle.0);
            });
        }
    }

    // See distlib/PC/launcher.c::cleanup_fds()
    if si.cbReserved2 == 0 || si.lpReserved2.is_null() {
        return;
    }

    let crt_magic = si.lpReserved2 as *const u32;
    let handle_count = unsafe { crt_magic.read_unaligned() } as isize;
    let handle_start =
        unsafe { (crt_magic.offset(1) as *const u8).offset(handle_count) as *const HANDLE };

    // Close all fds inherited from the parent, except for the standard I/O fds (skip first 3).
    for i in 3..handle_count {
        let handle = unsafe { handle_start.offset(i).read_unaligned() };
        // Ignore invalid handles, as that means this fd was not inherited.
        // -2 is a special value (https://docs.microsoft.com/en-us/cpp/c-runtime-library/reference/get-osfhandle)
        if handle.is_invalid() || handle.0 == -2 as _ {
            continue;
        }
        unsafe { CloseHandle(handle) }.unwrap_or_else(|_| {
            warn!("Failed to close child file descriptors at {}", i);
        });
    }
}

/*
    I don't really understand what this function does. It's a straight port from
    https://github.com/pypa/distlib/blob/master/PC/launcher.c, which has the following
    comment:

        End the launcher's "app starting" cursor state.
        When Explorer launches a Windows (GUI) application, it displays
        the "app starting" (the "pointer + hourglass") cursor for a number
        of seconds, or until the app does something UI-ish (eg, creating a
        window, or fetching a message).  As this launcher doesn't do this
        directly, that cursor remains even after the child process does these
        things.  We avoid that by doing the stuff in here.
        See http://bugs.python.org/issue17290 and
        https://github.com/pypa/pip/issues/10444#issuecomment-973408601

    Why do we call `PostMessage`/`GetMessage` at the start, before waiting for the
    child? (Looking at the bpo issue above, this was originally the *whole* fix.)
    Is creating a window and calling PeekMessage the best way to do this? idk.
*/
fn clear_app_starting_state(child_handle: HANDLE) {
    let mut msg = MSG::default();
    unsafe {
        // End the launcher's "app starting" cursor state.
        PostMessageA(None, 0, WPARAM(0), LPARAM(0)).unwrap_or_else(|_| {
            warn!("Failed to post a message to specified window");
        });
        if GetMessageA(&mut msg, None, 0, 0) != TRUE {
            warn!("Failed to retrieve posted window message");
        }
        // Proxy the child's input idle event.
        if WaitForInputIdle(child_handle, INFINITE) != 0 {
            warn!("Failed to wait for input from window");
        }
        // Signal the process input idle event by creating a window and pumping
        // sent messages. The window class isn't important, so just use the
        // system "STATIC" class.
        if let Ok(hwnd) = CreateWindowExA(
            WINDOW_EX_STYLE(0),
            s!("STATIC"),
            s!("uv Python Trampoline"),
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            Some(HWND_MESSAGE),
            None,
            None,
            None,
        ) {
            // Process all sent messages and signal input idle.
            let _ = PeekMessageA(&mut msg, Some(hwnd), 0, 0, PEEK_MESSAGE_REMOVE_TYPE(0));
            DestroyWindow(hwnd).unwrap_or_else(|_| {
                print_last_error_and_exit("Failed to destroy temporary window");
            });
        }
    }
}

pub fn bounce(is_gui: bool) -> ! {
    let child_cmdline = make_child_cmdline();

    let mut si = STARTUPINFOA::default();
    unsafe { GetStartupInfoA(&mut si) }

    let child_handle = spawn_child(&si, child_cmdline);
    let job = make_job_object();

    if unsafe { AssignProcessToJobObject(job, child_handle) }.is_err() {
        print_last_error_and_exit("Failed to assign child process to the job")
    }

    // (best effort) Close all the handles that we can
    close_handles(&si);

    // (best effort) Switch to some innocuous directory, so we don't hold the original cwd open.
    // See distlib/PC/launcher.c::switch_working_directory
    if std::env::set_current_dir(std::env::temp_dir()).is_err() {
        warn!("Failed to set cwd to temp dir");
    }

    // We want to ignore control-C/control-Break/logout/etc.; the same event will
    // be delivered to the child, so we let them decide whether to exit or not.
    unsafe extern "system" fn control_key_handler(_: u32) -> BOOL {
        TRUE
    }
    // See distlib/PC/launcher.c::control_key_handler
    unsafe { SetConsoleCtrlHandler(Some(control_key_handler), true) }.unwrap_or_else(|_| {
        print_last_error_and_exit("Control handler setting failed");
    });

    if is_gui {
        clear_app_starting_state(child_handle);
    }

    let _ = unsafe { WaitForSingleObject(child_handle, INFINITE) };
    let mut exit_code = 0u32;
    if unsafe { GetExitCodeProcess(child_handle, &mut exit_code) }.is_err() {
        print_last_error_and_exit("Failed to get exit code of child process");
    }
    exit_with_status(exit_code);
}

#[cold]
fn error_and_exit(message: &str) -> ! {
    error!("{}", message);
    exit_with_status(1);
}

#[cold]
fn print_last_error_and_exit(message: &str) -> ! {
    let err = std::io::Error::last_os_error();
    let err_no_str = err
        .raw_os_error()
        .map(|raw_error| format!(" (os error {})", raw_error))
        .unwrap_or_default();
    // we can't access sys::os::error_string directly so err.kind().to_string()
    // is the closest we can get to while avoiding bringing in a large chunk of core::fmt
    let message = format!(
        "(uv internal error) {}: {}.{}",
        message,
        err.kind().to_string(),
        err_no_str
    );
    error_and_exit(&message);
}

#[cold]
fn exit_with_status(code: u32) -> ! {
    // ~5-10kb
    // Pulls in core::fmt::{write, Write, getcount}
    std::process::exit(code as _)
}
