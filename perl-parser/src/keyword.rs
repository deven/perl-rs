//! Keyword table — maps identifier strings to `Keyword` variants.

use crate::token::Keyword;

/// Look up a keyword by name.  Returns `None` if not a keyword.
pub fn lookup_keyword(name: &str) -> Option<Keyword> {
    // Sorted by frequency of use for fast early-exit in a match.
    // (A perfect hash or phf would be better for production, but
    // a match is simpler and correct for bootstrapping.)
    match name {
        "my" => Some(Keyword::My),
        "sub" => Some(Keyword::Sub),
        "if" => Some(Keyword::If),
        "else" => Some(Keyword::Else),
        "elsif" => Some(Keyword::Elsif),
        "elseif" => Some(Keyword::Elseif),
        "unless" => Some(Keyword::Unless),
        "while" => Some(Keyword::While),
        "until" => Some(Keyword::Until),
        "for" => Some(Keyword::For),
        "foreach" => Some(Keyword::Foreach),
        "return" => Some(Keyword::Return),
        "use" => Some(Keyword::Use),
        "no" => Some(Keyword::No),
        "our" => Some(Keyword::Our),
        "local" => Some(Keyword::Local),
        "state" => Some(Keyword::State),
        "do" => Some(Keyword::Do),
        "require" => Some(Keyword::Require),
        "package" => Some(Keyword::Package),
        "class" => Some(Keyword::Class),
        "field" => Some(Keyword::Field),
        "method" => Some(Keyword::Method),
        "and" => Some(Keyword::And),
        "or" => Some(Keyword::Or),
        "not" => Some(Keyword::Not),
        "undef" => Some(Keyword::Undef),
        "die" => Some(Keyword::Die),
        "warn" => Some(Keyword::Warn),
        "eval" => Some(Keyword::Eval),
        "print" => Some(Keyword::Print),
        "say" => Some(Keyword::Say),
        "last" => Some(Keyword::Last),
        "next" => Some(Keyword::Next),
        "redo" => Some(Keyword::Redo),
        "goto" => Some(Keyword::Goto),
        "dump" => Some(Keyword::Dump),
        "defined" => Some(Keyword::Defined),
        "ref" => Some(Keyword::Ref),
        "exists" => Some(Keyword::Exists),
        "delete" => Some(Keyword::Delete),
        "push" => Some(Keyword::Push),
        "pop" => Some(Keyword::Pop),
        "shift" => Some(Keyword::Shift),
        "unshift" => Some(Keyword::Unshift),
        "splice" => Some(Keyword::Splice),
        "keys" => Some(Keyword::Keys),
        "values" => Some(Keyword::Values),
        "each" => Some(Keyword::Each),
        "reverse" => Some(Keyword::Reverse),
        "sort" => Some(Keyword::Sort),
        "map" => Some(Keyword::Map),
        "grep" => Some(Keyword::Grep),
        "any" => Some(Keyword::Any),
        "all" => Some(Keyword::All),
        "join" => Some(Keyword::Join),
        "split" => Some(Keyword::Split),
        "sprintf" => Some(Keyword::Sprintf),
        "printf" => Some(Keyword::Printf),
        "chomp" => Some(Keyword::Chomp),
        "chop" => Some(Keyword::Chop),
        "chr" => Some(Keyword::Chr),
        "ord" => Some(Keyword::Ord),
        "hex" => Some(Keyword::Hex),
        "oct" => Some(Keyword::Oct),
        "lc" => Some(Keyword::Lc),
        "uc" => Some(Keyword::Uc),
        "lcfirst" => Some(Keyword::Lcfirst),
        "ucfirst" => Some(Keyword::Ucfirst),
        "length" => Some(Keyword::Length),
        "substr" => Some(Keyword::Substr),
        "index" => Some(Keyword::Index),
        "rindex" => Some(Keyword::Rindex),
        "abs" => Some(Keyword::Abs),
        "int" => Some(Keyword::Int),
        "sqrt" => Some(Keyword::Sqrt),
        "rand" => Some(Keyword::Rand),
        "srand" => Some(Keyword::Srand),
        "wantarray" => Some(Keyword::Wantarray),
        "scalar" => Some(Keyword::Scalar),
        "caller" => Some(Keyword::Caller),
        "exit" => Some(Keyword::Exit),
        "chdir" => Some(Keyword::Chdir),
        "mkdir" => Some(Keyword::Mkdir),
        "rmdir" => Some(Keyword::Rmdir),
        "unlink" => Some(Keyword::Unlink),
        "rename" => Some(Keyword::Rename),
        "open" => Some(Keyword::Open),
        "close" => Some(Keyword::Close),
        "read" => Some(Keyword::Read),
        "write" => Some(Keyword::Write),
        "seek" => Some(Keyword::Seek),
        "select" => Some(Keyword::Select),
        "tell" => Some(Keyword::Tell),
        "eof" => Some(Keyword::Eof),
        "getc" => Some(Keyword::Getc),
        "readline" => Some(Keyword::Readline),
        "readlink" => Some(Keyword::Readlink),
        "binmode" => Some(Keyword::Binmode),
        "stat" => Some(Keyword::Stat),
        "lstat" => Some(Keyword::Lstat),
        "chmod" => Some(Keyword::Chmod),
        "chown" => Some(Keyword::Chown),
        "umask" => Some(Keyword::Umask),
        "glob" => Some(Keyword::Glob),
        "opendir" => Some(Keyword::Opendir),
        "readdir" => Some(Keyword::Readdir),
        "closedir" => Some(Keyword::Closedir),
        "pos" => Some(Keyword::Pos),
        "system" => Some(Keyword::System),
        "exec" => Some(Keyword::Exec),
        // Named unary builtins (additional)
        "sleep" => Some(Keyword::Sleep),
        "alarm" => Some(Keyword::Alarm),
        "localtime" => Some(Keyword::Localtime),
        "gmtime" => Some(Keyword::Gmtime),
        "sin" => Some(Keyword::Sin),
        "cos" => Some(Keyword::Cos),
        "exp" => Some(Keyword::Exp),
        "log" => Some(Keyword::Log),
        "quotemeta" => Some(Keyword::Quotemeta),
        "prototype" => Some(Keyword::Prototype),
        "readpipe" => Some(Keyword::Readpipe),
        "chroot" => Some(Keyword::Chroot),
        "reset" => Some(Keyword::Reset),
        // Named unary — database lookup (single arg)
        "getpwnam" => Some(Keyword::Getpwnam),
        "getgrnam" => Some(Keyword::Getgrnam),
        "gethostbyname" => Some(Keyword::Gethostbyname),
        "getnetbyname" => Some(Keyword::Getnetbyname),
        "getprotobyname" => Some(Keyword::Getprotobyname),
        "getpwuid" => Some(Keyword::Getpwuid),
        "getgrgid" => Some(Keyword::Getgrgid),
        "getprotobynumber" => Some(Keyword::Getprotobynumber),
        // List operators — system/process
        "waitpid" => Some(Keyword::Waitpid),
        "kill" => Some(Keyword::Kill),
        "pipe" => Some(Keyword::Pipe),
        "setpgrp" => Some(Keyword::Setpgrp),
        "setpriority" => Some(Keyword::Setpriority),
        "getpriority" => Some(Keyword::Getpriority),
        "syscall" => Some(Keyword::Syscall),
        // List operators — socket/network
        "socket" => Some(Keyword::Socket),
        "socketpair" => Some(Keyword::Socketpair),
        "bind" => Some(Keyword::Bind),
        "connect" => Some(Keyword::Connect),
        "listen" => Some(Keyword::Listen),
        "accept" => Some(Keyword::Accept),
        "shutdown" => Some(Keyword::Shutdown),
        "send" => Some(Keyword::Send),
        "recv" => Some(Keyword::Recv),
        "setsockopt" => Some(Keyword::Setsockopt),
        "getsockopt" => Some(Keyword::Getsockopt),
        // List operators — SysV IPC
        "shmget" => Some(Keyword::Shmget),
        "shmctl" => Some(Keyword::Shmctl),
        "shmread" => Some(Keyword::Shmread),
        "shmwrite" => Some(Keyword::Shmwrite),
        "semget" => Some(Keyword::Semget),
        "semctl" => Some(Keyword::Semctl),
        "semop" => Some(Keyword::Semop),
        "msgget" => Some(Keyword::Msgget),
        "msgctl" => Some(Keyword::Msgctl),
        "msgsnd" => Some(Keyword::Msgsnd),
        "msgrcv" => Some(Keyword::Msgrcv),
        // List operators — database lookup (multi-arg)
        "getservbyname" => Some(Keyword::Getservbyname),
        "gethostbyaddr" => Some(Keyword::Gethostbyaddr),
        "getnetbyaddr" => Some(Keyword::Getnetbyaddr),
        "getservbyport" => Some(Keyword::Getservbyport),
        // List operators — low-level I/O
        "sysopen" => Some(Keyword::Sysopen),
        "sysread" => Some(Keyword::Sysread),
        "syswrite" => Some(Keyword::Syswrite),
        "sysseek" => Some(Keyword::Sysseek),
        "truncate" => Some(Keyword::Truncate),
        "fcntl" => Some(Keyword::Fcntl),
        "ioctl" => Some(Keyword::Ioctl),
        "flock" => Some(Keyword::Flock),
        "seekdir" => Some(Keyword::Seekdir),
        // List operators — file ops
        "link" => Some(Keyword::Link),
        "symlink" => Some(Keyword::Symlink),
        "utime" => Some(Keyword::Utime),
        // List operators — data
        "pack" => Some(Keyword::Pack),
        "unpack" => Some(Keyword::Unpack),
        "vec" => Some(Keyword::Vec),
        "formline" => Some(Keyword::Formline),
        "qw" => Some(Keyword::Qw),
        "format" => Some(Keyword::Format),
        "BEGIN" => Some(Keyword::BEGIN),
        "END" => Some(Keyword::END),
        "INIT" => Some(Keyword::INIT),
        "CHECK" => Some(Keyword::CHECK),
        "UNITCHECK" => Some(Keyword::UNITCHECK),
        "ADJUST" => Some(Keyword::ADJUST),
        "given" => Some(Keyword::Given),
        "when" => Some(Keyword::When),
        "default" => Some(Keyword::Default),
        "try" => Some(Keyword::Try),
        "catch" => Some(Keyword::Catch),
        "finally" => Some(Keyword::Finally),
        "defer" => Some(Keyword::Defer),
        "continue" => Some(Keyword::Continue),
        "tie" => Some(Keyword::Tie),
        "untie" => Some(Keyword::Untie),
        "tied" => Some(Keyword::Tied),
        "bless" => Some(Keyword::Bless),
        // Nullary builtins (no arguments)
        "time" => Some(Keyword::Time),
        "times" => Some(Keyword::Times),
        "fork" => Some(Keyword::Fork),
        "wait" => Some(Keyword::Wait),
        "getppid" => Some(Keyword::Getppid),
        "getlogin" => Some(Keyword::Getlogin),
        "setpwent" => Some(Keyword::Setpwent),
        "setgrent" => Some(Keyword::Setgrent),
        "endpwent" => Some(Keyword::Endpwent),
        "endgrent" => Some(Keyword::Endgrent),
        "endhostent" => Some(Keyword::Endhostent),
        "endnetent" => Some(Keyword::Endnetent),
        "endprotoent" => Some(Keyword::Endprotoent),
        "endservent" => Some(Keyword::Endservent),
        "getpwent" => Some(Keyword::Getpwent),
        "getgrent" => Some(Keyword::Getgrent),
        "gethostent" => Some(Keyword::Gethostent),
        "getnetent" => Some(Keyword::Getnetent),
        "getprotoent" => Some(Keyword::Getprotoent),
        "getservent" => Some(Keyword::Getservent),
        // String comparison operators (keywords, not symbols)
        "eq" => Some(Keyword::Eq),
        "ne" => Some(Keyword::Ne),
        "lt" => Some(Keyword::Lt),
        "gt" => Some(Keyword::Gt),
        "le" => Some(Keyword::Le),
        "ge" => Some(Keyword::Ge),
        "cmp" => Some(Keyword::Cmp),
        // Typed layer (our extensions)
        "let" => Some(Keyword::Let),
        "fn" => Some(Keyword::Fn),
        "struct" => Some(Keyword::Struct),
        "enum" => Some(Keyword::Enum),
        "impl" => Some(Keyword::Impl),
        "trait" => Some(Keyword::Trait),
        "match" => Some(Keyword::Match),
        _ => None,
    }
}

/// Reverse lookup: get the source-text name of a keyword.
impl From<Keyword> for &'static str {
    fn from(kw: Keyword) -> &'static str {
        match kw {
            Keyword::My => "my",
            Keyword::Sub => "sub",
            Keyword::If => "if",
            Keyword::Else => "else",
            Keyword::Elsif => "elsif",
            Keyword::Elseif => "elseif",
            Keyword::Unless => "unless",
            Keyword::While => "while",
            Keyword::Until => "until",
            Keyword::For => "for",
            Keyword::Foreach => "foreach",
            Keyword::Return => "return",
            Keyword::Use => "use",
            Keyword::No => "no",
            Keyword::Our => "our",
            Keyword::Local => "local",
            Keyword::State => "state",
            Keyword::Do => "do",
            Keyword::Require => "require",
            Keyword::Package => "package",
            Keyword::Class => "class",
            Keyword::Field => "field",
            Keyword::Method => "method",
            Keyword::And => "and",
            Keyword::Or => "or",
            Keyword::Not => "not",
            Keyword::Undef => "undef",
            Keyword::Die => "die",
            Keyword::Warn => "warn",
            Keyword::Eval => "eval",
            Keyword::Print => "print",
            Keyword::Say => "say",
            Keyword::Last => "last",
            Keyword::Next => "next",
            Keyword::Redo => "redo",
            Keyword::Goto => "goto",
            Keyword::Dump => "dump",
            Keyword::Defined => "defined",
            Keyword::Ref => "ref",
            Keyword::Exists => "exists",
            Keyword::Delete => "delete",
            Keyword::Push => "push",
            Keyword::Pop => "pop",
            Keyword::Shift => "shift",
            Keyword::Unshift => "unshift",
            Keyword::Splice => "splice",
            Keyword::Keys => "keys",
            Keyword::Values => "values",
            Keyword::Each => "each",
            Keyword::Reverse => "reverse",
            Keyword::Sort => "sort",
            Keyword::Map => "map",
            Keyword::Grep => "grep",
            Keyword::Any => "any",
            Keyword::All => "all",
            Keyword::Join => "join",
            Keyword::Split => "split",
            Keyword::Sprintf => "sprintf",
            Keyword::Printf => "printf",
            Keyword::Chomp => "chomp",
            Keyword::Chop => "chop",
            Keyword::Chr => "chr",
            Keyword::Ord => "ord",
            Keyword::Hex => "hex",
            Keyword::Oct => "oct",
            Keyword::Lc => "lc",
            Keyword::Uc => "uc",
            Keyword::Lcfirst => "lcfirst",
            Keyword::Ucfirst => "ucfirst",
            Keyword::Length => "length",
            Keyword::Substr => "substr",
            Keyword::Index => "index",
            Keyword::Rindex => "rindex",
            Keyword::Abs => "abs",
            Keyword::Int => "int",
            Keyword::Sqrt => "sqrt",
            Keyword::Rand => "rand",
            Keyword::Srand => "srand",
            Keyword::Wantarray => "wantarray",
            Keyword::Scalar => "scalar",
            Keyword::Caller => "caller",
            Keyword::Exit => "exit",
            Keyword::Chdir => "chdir",
            Keyword::Mkdir => "mkdir",
            Keyword::Rmdir => "rmdir",
            Keyword::Unlink => "unlink",
            Keyword::Rename => "rename",
            Keyword::Open => "open",
            Keyword::Close => "close",
            Keyword::Read => "read",
            Keyword::Write => "write",
            Keyword::Seek => "seek",
            Keyword::Select => "select",
            Keyword::Tell => "tell",
            Keyword::Eof => "eof",
            Keyword::Getc => "getc",
            Keyword::Readline => "readline",
            Keyword::Readlink => "readlink",
            Keyword::Binmode => "binmode",
            Keyword::Stat => "stat",
            Keyword::Lstat => "lstat",
            Keyword::Chmod => "chmod",
            Keyword::Chown => "chown",
            Keyword::Umask => "umask",
            Keyword::Glob => "glob",
            Keyword::Opendir => "opendir",
            Keyword::Readdir => "readdir",
            Keyword::Closedir => "closedir",
            Keyword::Pos => "pos",
            Keyword::System => "system",
            Keyword::Exec => "exec",
            Keyword::Sleep => "sleep",
            Keyword::Alarm => "alarm",
            Keyword::Localtime => "localtime",
            Keyword::Gmtime => "gmtime",
            Keyword::Sin => "sin",
            Keyword::Cos => "cos",
            Keyword::Exp => "exp",
            Keyword::Log => "log",
            Keyword::Quotemeta => "quotemeta",
            Keyword::Prototype => "prototype",
            Keyword::Readpipe => "readpipe",
            Keyword::Chroot => "chroot",
            Keyword::Reset => "reset",
            Keyword::Getpwnam => "getpwnam",
            Keyword::Getgrnam => "getgrnam",
            Keyword::Gethostbyname => "gethostbyname",
            Keyword::Getnetbyname => "getnetbyname",
            Keyword::Getprotobyname => "getprotobyname",
            Keyword::Getpwuid => "getpwuid",
            Keyword::Getgrgid => "getgrgid",
            Keyword::Getprotobynumber => "getprotobynumber",
            Keyword::Waitpid => "waitpid",
            Keyword::Kill => "kill",
            Keyword::Pipe => "pipe",
            Keyword::Setpgrp => "setpgrp",
            Keyword::Setpriority => "setpriority",
            Keyword::Getpriority => "getpriority",
            Keyword::Syscall => "syscall",
            Keyword::Socket => "socket",
            Keyword::Socketpair => "socketpair",
            Keyword::Bind => "bind",
            Keyword::Connect => "connect",
            Keyword::Listen => "listen",
            Keyword::Accept => "accept",
            Keyword::Shutdown => "shutdown",
            Keyword::Send => "send",
            Keyword::Recv => "recv",
            Keyword::Setsockopt => "setsockopt",
            Keyword::Getsockopt => "getsockopt",
            Keyword::Shmget => "shmget",
            Keyword::Shmctl => "shmctl",
            Keyword::Shmread => "shmread",
            Keyword::Shmwrite => "shmwrite",
            Keyword::Semget => "semget",
            Keyword::Semctl => "semctl",
            Keyword::Semop => "semop",
            Keyword::Msgget => "msgget",
            Keyword::Msgctl => "msgctl",
            Keyword::Msgsnd => "msgsnd",
            Keyword::Msgrcv => "msgrcv",
            Keyword::Getservbyname => "getservbyname",
            Keyword::Gethostbyaddr => "gethostbyaddr",
            Keyword::Getnetbyaddr => "getnetbyaddr",
            Keyword::Getservbyport => "getservbyport",
            Keyword::Sysopen => "sysopen",
            Keyword::Sysread => "sysread",
            Keyword::Syswrite => "syswrite",
            Keyword::Sysseek => "sysseek",
            Keyword::Truncate => "truncate",
            Keyword::Fcntl => "fcntl",
            Keyword::Ioctl => "ioctl",
            Keyword::Flock => "flock",
            Keyword::Seekdir => "seekdir",
            Keyword::Link => "link",
            Keyword::Symlink => "symlink",
            Keyword::Utime => "utime",
            Keyword::Pack => "pack",
            Keyword::Unpack => "unpack",
            Keyword::Vec => "vec",
            Keyword::Formline => "formline",
            Keyword::Qw => "qw",
            Keyword::Format => "format",
            Keyword::BEGIN => "BEGIN",
            Keyword::END => "END",
            Keyword::INIT => "INIT",
            Keyword::CHECK => "CHECK",
            Keyword::UNITCHECK => "UNITCHECK",
            Keyword::ADJUST => "ADJUST",
            Keyword::Given => "given",
            Keyword::When => "when",
            Keyword::Default => "default",
            Keyword::Try => "try",
            Keyword::Catch => "catch",
            Keyword::Finally => "finally",
            Keyword::Defer => "defer",
            Keyword::Continue => "continue",
            Keyword::Tie => "tie",
            Keyword::Untie => "untie",
            Keyword::Tied => "tied",
            Keyword::Bless => "bless",
            Keyword::Blessed => "blessed",
            Keyword::Time => "time",
            Keyword::Times => "times",
            Keyword::Fork => "fork",
            Keyword::Wait => "wait",
            Keyword::Getppid => "getppid",
            Keyword::Getlogin => "getlogin",
            Keyword::Setpwent => "setpwent",
            Keyword::Setgrent => "setgrent",
            Keyword::Endpwent => "endpwent",
            Keyword::Endgrent => "endgrent",
            Keyword::Endhostent => "endhostent",
            Keyword::Endnetent => "endnetent",
            Keyword::Endprotoent => "endprotoent",
            Keyword::Endservent => "endservent",
            Keyword::Getpwent => "getpwent",
            Keyword::Getgrent => "getgrent",
            Keyword::Gethostent => "gethostent",
            Keyword::Getnetent => "getnetent",
            Keyword::Getprotoent => "getprotoent",
            Keyword::Getservent => "getservent",
            Keyword::Die_ => "die",
            Keyword::Eq => "eq",
            Keyword::Ne => "ne",
            Keyword::Lt => "lt",
            Keyword::Gt => "gt",
            Keyword::Le => "le",
            Keyword::Ge => "ge",
            Keyword::Cmp => "cmp",
            Keyword::Let => "let",
            Keyword::Fn => "fn",
            Keyword::Struct => "struct",
            Keyword::Enum => "enum",
            Keyword::Impl => "impl",
            Keyword::Trait => "trait",
            Keyword::Match => "match",
        }
    }
}

/// Is this keyword a named unary operator?
/// After a named unary, the next thing is a term (so `/` is regex).
pub fn is_named_unary(kw: Keyword) -> bool {
    matches!(
        kw,
        Keyword::Defined
            | Keyword::Ref
            | Keyword::Exists
            | Keyword::Delete
            | Keyword::Chomp
            | Keyword::Chop
            | Keyword::Chr
            | Keyword::Ord
            | Keyword::Hex
            | Keyword::Oct
            | Keyword::Lc
            | Keyword::Uc
            | Keyword::Lcfirst
            | Keyword::Ucfirst
            | Keyword::Length
            | Keyword::Abs
            | Keyword::Int
            | Keyword::Sqrt
            | Keyword::Rand
            | Keyword::Srand
            | Keyword::Caller
            | Keyword::Eof
            | Keyword::Getc
            | Keyword::Readline
            | Keyword::Readlink
            | Keyword::Rmdir
            | Keyword::Chdir
            | Keyword::Close
            | Keyword::Closedir
            | Keyword::Readdir
            | Keyword::Pop
            | Keyword::Shift
            | Keyword::Pos
            | Keyword::Umask
            | Keyword::Exit
            | Keyword::Tied
            | Keyword::Die
            | Keyword::Warn
            | Keyword::Undef
            | Keyword::Scalar
            | Keyword::Require
            | Keyword::Sleep
            | Keyword::Alarm
            | Keyword::Localtime
            | Keyword::Gmtime
            | Keyword::Sin
            | Keyword::Cos
            | Keyword::Exp
            | Keyword::Log
            | Keyword::Quotemeta
            | Keyword::Prototype
            | Keyword::Readpipe
            | Keyword::Chroot
            | Keyword::Reset
            | Keyword::Getpwnam
            | Keyword::Getgrnam
            | Keyword::Gethostbyname
            | Keyword::Getnetbyname
            | Keyword::Getprotobyname
            | Keyword::Getpwuid
            | Keyword::Getgrgid
            | Keyword::Getprotobynumber
    )
}

/// Is this keyword a nullary builtin (takes no arguments)?
///
/// Nullary builtins never consume a following term as an argument,
/// so `time+86_400` is always `time() + 86_400`.  They do accept
/// explicit empty parens: `time()`.
pub fn is_nullary(kw: Keyword) -> bool {
    matches!(
        kw,
        Keyword::Wantarray
            | Keyword::Time
            | Keyword::Times
            | Keyword::Fork
            | Keyword::Wait
            | Keyword::Getppid
            | Keyword::Getlogin
            | Keyword::Setpwent
            | Keyword::Setgrent
            | Keyword::Endpwent
            | Keyword::Endgrent
            | Keyword::Endhostent
            | Keyword::Endnetent
            | Keyword::Endprotoent
            | Keyword::Endservent
            | Keyword::Getpwent
            | Keyword::Getgrent
            | Keyword::Gethostent
            | Keyword::Getnetent
            | Keyword::Getprotoent
            | Keyword::Getservent
    )
}

/// Does this operator prefer `//` as defined-or over an empty regex argument?
///
/// Matches toke.c's `UNIDOR` macro: shift, pop, getc, pos, readline,
/// readlink, undef, umask.  After these operators, `shift // 0` is
/// parsed as `shift() // 0`, not `shift(m//)`.
pub fn prefers_defined_or(kw: Keyword) -> bool {
    matches!(kw, Keyword::Shift | Keyword::Pop | Keyword::Getc | Keyword::Pos | Keyword::Readline | Keyword::Readlink | Keyword::Undef | Keyword::Umask)
}

/// Is this keyword a list operator?
/// After a list op, the next thing is a term (so `/` is regex).
pub fn is_list_op(kw: Keyword) -> bool {
    matches!(
        kw,
        Keyword::Push
            | Keyword::Unshift
            | Keyword::Splice
            | Keyword::Keys
            | Keyword::Values
            | Keyword::Each
            | Keyword::Reverse
            | Keyword::Join
            | Keyword::Split
            | Keyword::Sprintf
            | Keyword::Substr
            | Keyword::Index
            | Keyword::Rindex
            | Keyword::Chmod
            | Keyword::Chown
            | Keyword::Unlink
            | Keyword::Rename
            | Keyword::Open
            | Keyword::Read
            | Keyword::Write
            | Keyword::Seek
            | Keyword::Tell
            | Keyword::Binmode
            | Keyword::Glob
            | Keyword::Opendir
            | Keyword::System
            | Keyword::Exec
            | Keyword::Tie
            | Keyword::Untie
            | Keyword::Bless
            | Keyword::Mkdir
            | Keyword::Select
            // System/process
            | Keyword::Waitpid
            | Keyword::Kill
            | Keyword::Pipe
            | Keyword::Setpgrp
            | Keyword::Setpriority
            | Keyword::Getpriority
            | Keyword::Syscall
            // Socket/network
            | Keyword::Socket
            | Keyword::Socketpair
            | Keyword::Bind
            | Keyword::Connect
            | Keyword::Listen
            | Keyword::Accept
            | Keyword::Shutdown
            | Keyword::Send
            | Keyword::Recv
            | Keyword::Setsockopt
            | Keyword::Getsockopt
            // SysV IPC
            | Keyword::Shmget
            | Keyword::Shmctl
            | Keyword::Shmread
            | Keyword::Shmwrite
            | Keyword::Semget
            | Keyword::Semctl
            | Keyword::Semop
            | Keyword::Msgget
            | Keyword::Msgctl
            | Keyword::Msgsnd
            | Keyword::Msgrcv
            // Database lookup (multi-arg)
            | Keyword::Getservbyname
            | Keyword::Gethostbyaddr
            | Keyword::Getnetbyaddr
            | Keyword::Getservbyport
            // Low-level I/O
            | Keyword::Sysopen
            | Keyword::Sysread
            | Keyword::Syswrite
            | Keyword::Sysseek
            | Keyword::Truncate
            | Keyword::Fcntl
            | Keyword::Ioctl
            | Keyword::Flock
            | Keyword::Seekdir
            // File ops
            | Keyword::Link
            | Keyword::Symlink
            | Keyword::Utime
            // Data
            | Keyword::Pack
            | Keyword::Unpack
            | Keyword::Vec
            | Keyword::Formline
    )
}

/// Is this keyword a block-list operator (sort/map/grep)?
/// These take an optional block as the first argument.
pub fn is_block_list_op(kw: Keyword) -> bool {
    matches!(kw, Keyword::Sort | Keyword::Map | Keyword::Grep | Keyword::Any | Keyword::All)
}

/// Is this keyword a print-like operator?
/// These take an optional filehandle as the first argument.
pub fn is_print_op(kw: Keyword) -> bool {
    matches!(kw, Keyword::Print | Keyword::Say | Keyword::Printf)
}

/// Keywords that have dedicated statement-level handlers in the parser.
/// These are consumed before dispatching, so they need the fat comma
/// check at the statement level rather than in parse_term.
pub fn is_statement_keyword(kw: Keyword) -> bool {
    matches!(
        kw,
        Keyword::My
            | Keyword::Our
            | Keyword::State
            | Keyword::Sub
            | Keyword::If
            | Keyword::Unless
            | Keyword::While
            | Keyword::Until
            | Keyword::For
            | Keyword::Foreach
            | Keyword::Package
            | Keyword::Use
            | Keyword::No
            | Keyword::BEGIN
            | Keyword::END
            | Keyword::INIT
            | Keyword::CHECK
            | Keyword::UNITCHECK
            | Keyword::ADJUST
            | Keyword::Given
            | Keyword::When
            | Keyword::Default
            | Keyword::Try
            | Keyword::Defer
            | Keyword::Format
            | Keyword::Class
            | Keyword::Field
            | Keyword::Method
    )
}
