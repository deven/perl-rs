//! Keyword table — maps identifier strings to `Keyword` variants.

/// Perl keyword.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Keyword {
    __FILE__,
    __LINE__,
    __PACKAGE__,
    __CLASS__,
    __DATA__,
    __END__,
    __SUB__,
    ADJUST,
    AUTOLOAD,
    BEGIN,
    UNITCHECK,
    DESTROY,
    END,
    INIT,
    CHECK,
    Abs,
    Accept,
    Alarm,
    All,
    And,
    Any,
    Atan2,
    Bind,
    Binmode,
    Bless,
    Break,
    Caller,
    Catch,
    Chdir,
    Chmod,
    Chomp,
    Chop,
    Chown,
    Chr,
    Chroot,
    Class,
    Close,
    Closedir,
    Cmp,
    Connect,
    Continue,
    Cos,
    Crypt,
    Dbmclose,
    Dbmopen,
    Default,
    Defer,
    Defined,
    Delete,
    Die,
    Do,
    Dump,
    Each,
    Else,
    Elsif,
    Elseif, // parsed only to emit "elseif should be elsif" diagnostic
    Endgrent,
    Endhostent,
    Endnetent,
    Endprotoent,
    Endpwent,
    Endservent,
    Eof,
    Eq,
    Eval,
    Evalbytes,
    Exec,
    Exists,
    Exit,
    Exp,
    Fc,
    Fcntl,
    Field,
    Fileno,
    Finally,
    Flock,
    For,
    Foreach,
    Fork,
    Format,
    Formline,
    Ge,
    Getc,
    Getgrent,
    Getgrgid,
    Getgrnam,
    Gethostbyaddr,
    Gethostbyname,
    Gethostent,
    Getlogin,
    Getnetbyaddr,
    Getnetbyname,
    Getnetent,
    Getpeername,
    Getpgrp,
    Getppid,
    Getpriority,
    Getprotobyname,
    Getprotobynumber,
    Getprotoent,
    Getpwent,
    Getpwnam,
    Getpwuid,
    Getservbyname,
    Getservbyport,
    Getservent,
    Getsockname,
    Getsockopt,
    Given,
    Glob,
    Gmtime,
    Goto,
    Grep,
    Gt,
    Hex,
    If,
    Index,
    Int,
    Ioctl,
    Isa,
    Join,
    Keys,
    Kill,
    Last,
    Lc,
    Lcfirst,
    Le,
    Length,
    Link,
    Listen,
    Local,
    Localtime,
    Lock,
    Log,
    Lstat,
    Lt,
    M,
    Map,
    Method,
    Mkdir,
    Msgctl,
    Msgget,
    Msgrcv,
    Msgsnd,
    My,
    Ne,
    Next,
    No,
    Not,
    Oct,
    Open,
    Opendir,
    Or,
    Ord,
    Our,
    Pack,
    Package,
    Pipe,
    Pop,
    Pos,
    Print,
    Printf,
    Prototype,
    Push,
    Q,
    Qq,
    Qr,
    Quotemeta,
    Qw,
    Qx,
    Rand,
    Read,
    Readdir,
    Readline,
    Readlink,
    Readpipe,
    Recv,
    Redo,
    Ref,
    Rename,
    Require,
    Reset,
    Return,
    Reverse,
    Rewinddir,
    Rindex,
    Rmdir,
    S,
    Say,
    Scalar,
    Seek,
    Seekdir,
    Select,
    Semctl,
    Semget,
    Semop,
    Send,
    Setgrent,
    Sethostent,
    Setnetent,
    Setpgrp,
    Setpriority,
    Setprotoent,
    Setpwent,
    Setservent,
    Setsockopt,
    Shift,
    Shmctl,
    Shmget,
    Shmread,
    Shmwrite,
    Shutdown,
    Sin,
    Sleep,
    Socket,
    Socketpair,
    Sort,
    Splice,
    Split,
    Sprintf,
    Sqrt,
    Srand,
    Stat,
    State,
    Study,
    Sub,
    Substr,
    Symlink,
    Syscall,
    Sysopen,
    Sysread,
    Sysseek,
    System,
    Syswrite,
    Tell,
    Telldir,
    Tie,
    Tied,
    Time,
    Times,
    Tr,
    Try,
    Truncate,
    Uc,
    Ucfirst,
    Umask,
    Undef,
    Unless,
    Unlink,
    Unpack,
    Unshift,
    Untie,
    Until,
    Use,
    Utime,
    Values,
    Vec,
    Wait,
    Waitpid,
    Wantarray,
    Warn,
    When,
    While,
    Write,
    X,
    Xor,
    Y,
}

/// Look up a keyword by name.  Returns `None` if not a keyword.
pub fn lookup_keyword(name: &str) -> Option<Keyword> {
    // Sorted by frequency of use for fast early-exit in a match.  (A perfect hash or phf would be better for
    // production, but a match is simpler and correct for bootstrapping.)
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
        // Named unary — additional
        "fileno" => Some(Keyword::Fileno),
        "getpeername" => Some(Keyword::Getpeername),
        "getpgrp" => Some(Keyword::Getpgrp),
        "getsockname" => Some(Keyword::Getsockname),
        "rewinddir" => Some(Keyword::Rewinddir),
        "sethostent" => Some(Keyword::Sethostent),
        "setnetent" => Some(Keyword::Setnetent),
        "setprotoent" => Some(Keyword::Setprotoent),
        "setservent" => Some(Keyword::Setservent),
        "study" => Some(Keyword::Study),
        "telldir" => Some(Keyword::Telldir),
        "dbmclose" => Some(Keyword::Dbmclose),
        "lock" => Some(Keyword::Lock),
        "evalbytes" => Some(Keyword::Evalbytes),
        "fc" => Some(Keyword::Fc),
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
        // Additional list operators
        "atan2" => Some(Keyword::Atan2),
        "crypt" => Some(Keyword::Crypt),
        "dbmopen" => Some(Keyword::Dbmopen),
        "AUTOLOAD" => Some(Keyword::AUTOLOAD),
        "DESTROY" => Some(Keyword::DESTROY),
        // Infix operator keywords
        "x" => Some(Keyword::X),
        "xor" => Some(Keyword::Xor),
        "isa" => Some(Keyword::Isa),
        // Flow control
        "break" => Some(Keyword::Break),
        "qw" => Some(Keyword::Qw),
        "q" => Some(Keyword::Q),
        "qq" => Some(Keyword::Qq),
        "qr" => Some(Keyword::Qr),
        "qx" => Some(Keyword::Qx),
        "m" => Some(Keyword::M),
        "s" => Some(Keyword::S),
        "tr" => Some(Keyword::Tr),
        "y" => Some(Keyword::Y),
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
        "__PACKAGE__" => Some(Keyword::__PACKAGE__),
        "__SUB__" => Some(Keyword::__SUB__),
        "__CLASS__" => Some(Keyword::__CLASS__),
        _ => None,
    }
}

/// Reverse lookup: get the source-text name of a keyword.
impl From<Keyword> for &'static str {
    fn from(kw: Keyword) -> &'static str {
        match kw {
            Keyword::__FILE__ => "__FILE__",
            Keyword::__LINE__ => "__LINE__",
            Keyword::__PACKAGE__ => "__PACKAGE__",
            Keyword::__CLASS__ => "__CLASS__",
            Keyword::__DATA__ => "__DATA__",
            Keyword::__END__ => "__END__",
            Keyword::__SUB__ => "__SUB__",
            Keyword::ADJUST => "ADJUST",
            Keyword::AUTOLOAD => "AUTOLOAD",
            Keyword::BEGIN => "BEGIN",
            Keyword::UNITCHECK => "UNITCHECK",
            Keyword::DESTROY => "DESTROY",
            Keyword::END => "END",
            Keyword::INIT => "INIT",
            Keyword::CHECK => "CHECK",
            Keyword::Abs => "abs",
            Keyword::Accept => "accept",
            Keyword::Alarm => "alarm",
            Keyword::All => "all",
            Keyword::And => "and",
            Keyword::Any => "any",
            Keyword::Atan2 => "atan2",
            Keyword::Bind => "bind",
            Keyword::Binmode => "binmode",
            Keyword::Bless => "bless",
            Keyword::Break => "break",
            Keyword::Caller => "caller",
            Keyword::Catch => "catch",
            Keyword::Chdir => "chdir",
            Keyword::Chmod => "chmod",
            Keyword::Chomp => "chomp",
            Keyword::Chop => "chop",
            Keyword::Chown => "chown",
            Keyword::Chr => "chr",
            Keyword::Chroot => "chroot",
            Keyword::Class => "class",
            Keyword::Close => "close",
            Keyword::Closedir => "closedir",
            Keyword::Cmp => "cmp",
            Keyword::Connect => "connect",
            Keyword::Continue => "continue",
            Keyword::Cos => "cos",
            Keyword::Crypt => "crypt",
            Keyword::Dbmclose => "dbmclose",
            Keyword::Dbmopen => "dbmopen",
            Keyword::Default => "default",
            Keyword::Defer => "defer",
            Keyword::Defined => "defined",
            Keyword::Delete => "delete",
            Keyword::Die => "die",
            Keyword::Do => "do",
            Keyword::Dump => "dump",
            Keyword::Each => "each",
            Keyword::Else => "else",
            Keyword::Elsif => "elsif",
            Keyword::Elseif => "elseif",
            Keyword::Endgrent => "endgrent",
            Keyword::Endhostent => "endhostent",
            Keyword::Endnetent => "endnetent",
            Keyword::Endprotoent => "endprotoent",
            Keyword::Endpwent => "endpwent",
            Keyword::Endservent => "endservent",
            Keyword::Eof => "eof",
            Keyword::Eq => "eq",
            Keyword::Eval => "eval",
            Keyword::Evalbytes => "evalbytes",
            Keyword::Exec => "exec",
            Keyword::Exists => "exists",
            Keyword::Exit => "exit",
            Keyword::Exp => "exp",
            Keyword::Fc => "fc",
            Keyword::Fcntl => "fcntl",
            Keyword::Field => "field",
            Keyword::Fileno => "fileno",
            Keyword::Finally => "finally",
            Keyword::Flock => "flock",
            Keyword::For => "for",
            Keyword::Foreach => "foreach",
            Keyword::Fork => "fork",
            Keyword::Format => "format",
            Keyword::Formline => "formline",
            Keyword::Ge => "ge",
            Keyword::Getc => "getc",
            Keyword::Getgrent => "getgrent",
            Keyword::Getgrgid => "getgrgid",
            Keyword::Getgrnam => "getgrnam",
            Keyword::Gethostbyaddr => "gethostbyaddr",
            Keyword::Gethostbyname => "gethostbyname",
            Keyword::Gethostent => "gethostent",
            Keyword::Getlogin => "getlogin",
            Keyword::Getnetbyaddr => "getnetbyaddr",
            Keyword::Getnetbyname => "getnetbyname",
            Keyword::Getnetent => "getnetent",
            Keyword::Getpeername => "getpeername",
            Keyword::Getpgrp => "getpgrp",
            Keyword::Getppid => "getppid",
            Keyword::Getpriority => "getpriority",
            Keyword::Getprotobyname => "getprotobyname",
            Keyword::Getprotobynumber => "getprotobynumber",
            Keyword::Getprotoent => "getprotoent",
            Keyword::Getpwent => "getpwent",
            Keyword::Getpwnam => "getpwnam",
            Keyword::Getpwuid => "getpwuid",
            Keyword::Getservbyname => "getservbyname",
            Keyword::Getservbyport => "getservbyport",
            Keyword::Getservent => "getservent",
            Keyword::Getsockname => "getsockname",
            Keyword::Getsockopt => "getsockopt",
            Keyword::Given => "given",
            Keyword::Glob => "glob",
            Keyword::Gmtime => "gmtime",
            Keyword::Goto => "goto",
            Keyword::Grep => "grep",
            Keyword::Gt => "gt",
            Keyword::Hex => "hex",
            Keyword::If => "if",
            Keyword::Index => "index",
            Keyword::Int => "int",
            Keyword::Ioctl => "ioctl",
            Keyword::Isa => "isa",
            Keyword::Join => "join",
            Keyword::Keys => "keys",
            Keyword::Kill => "kill",
            Keyword::Last => "last",
            Keyword::Lc => "lc",
            Keyword::Lcfirst => "lcfirst",
            Keyword::Le => "le",
            Keyword::Length => "length",
            Keyword::Link => "link",
            Keyword::Listen => "listen",
            Keyword::Local => "local",
            Keyword::Localtime => "localtime",
            Keyword::Lock => "lock",
            Keyword::Log => "log",
            Keyword::Lstat => "lstat",
            Keyword::Lt => "lt",
            Keyword::M => "m",
            Keyword::Map => "map",
            Keyword::Method => "method",
            Keyword::Mkdir => "mkdir",
            Keyword::Msgctl => "msgctl",
            Keyword::Msgget => "msgget",
            Keyword::Msgrcv => "msgrcv",
            Keyword::Msgsnd => "msgsnd",
            Keyword::My => "my",
            Keyword::Ne => "ne",
            Keyword::Next => "next",
            Keyword::No => "no",
            Keyword::Not => "not",
            Keyword::Oct => "oct",
            Keyword::Open => "open",
            Keyword::Opendir => "opendir",
            Keyword::Or => "or",
            Keyword::Ord => "ord",
            Keyword::Our => "our",
            Keyword::Pack => "pack",
            Keyword::Package => "package",
            Keyword::Pipe => "pipe",
            Keyword::Pop => "pop",
            Keyword::Pos => "pos",
            Keyword::Print => "print",
            Keyword::Printf => "printf",
            Keyword::Prototype => "prototype",
            Keyword::Push => "push",
            Keyword::Q => "q",
            Keyword::Qq => "qq",
            Keyword::Qr => "qr",
            Keyword::Quotemeta => "quotemeta",
            Keyword::Qw => "qw",
            Keyword::Qx => "qx",
            Keyword::Rand => "rand",
            Keyword::Read => "read",
            Keyword::Readdir => "readdir",
            Keyword::Readline => "readline",
            Keyword::Readlink => "readlink",
            Keyword::Readpipe => "readpipe",
            Keyword::Recv => "recv",
            Keyword::Redo => "redo",
            Keyword::Ref => "ref",
            Keyword::Rename => "rename",
            Keyword::Require => "require",
            Keyword::Reset => "reset",
            Keyword::Return => "return",
            Keyword::Reverse => "reverse",
            Keyword::Rewinddir => "rewinddir",
            Keyword::Rindex => "rindex",
            Keyword::Rmdir => "rmdir",
            Keyword::S => "s",
            Keyword::Say => "say",
            Keyword::Scalar => "scalar",
            Keyword::Seek => "seek",
            Keyword::Seekdir => "seekdir",
            Keyword::Select => "select",
            Keyword::Semctl => "semctl",
            Keyword::Semget => "semget",
            Keyword::Semop => "semop",
            Keyword::Send => "send",
            Keyword::Setgrent => "setgrent",
            Keyword::Sethostent => "sethostent",
            Keyword::Setnetent => "setnetent",
            Keyword::Setpgrp => "setpgrp",
            Keyword::Setpriority => "setpriority",
            Keyword::Setprotoent => "setprotoent",
            Keyword::Setpwent => "setpwent",
            Keyword::Setservent => "setservent",
            Keyword::Setsockopt => "setsockopt",
            Keyword::Shift => "shift",
            Keyword::Shmctl => "shmctl",
            Keyword::Shmget => "shmget",
            Keyword::Shmread => "shmread",
            Keyword::Shmwrite => "shmwrite",
            Keyword::Shutdown => "shutdown",
            Keyword::Sin => "sin",
            Keyword::Sleep => "sleep",
            Keyword::Socket => "socket",
            Keyword::Socketpair => "socketpair",
            Keyword::Sort => "sort",
            Keyword::Splice => "splice",
            Keyword::Split => "split",
            Keyword::Sprintf => "sprintf",
            Keyword::Sqrt => "sqrt",
            Keyword::Srand => "srand",
            Keyword::Stat => "stat",
            Keyword::State => "state",
            Keyword::Study => "study",
            Keyword::Sub => "sub",
            Keyword::Substr => "substr",
            Keyword::Symlink => "symlink",
            Keyword::Syscall => "syscall",
            Keyword::Sysopen => "sysopen",
            Keyword::Sysread => "sysread",
            Keyword::Sysseek => "sysseek",
            Keyword::System => "system",
            Keyword::Syswrite => "syswrite",
            Keyword::Tell => "tell",
            Keyword::Telldir => "telldir",
            Keyword::Tie => "tie",
            Keyword::Tied => "tied",
            Keyword::Time => "time",
            Keyword::Times => "times",
            Keyword::Tr => "tr",
            Keyword::Try => "try",
            Keyword::Truncate => "truncate",
            Keyword::Uc => "uc",
            Keyword::Ucfirst => "ucfirst",
            Keyword::Umask => "umask",
            Keyword::Undef => "undef",
            Keyword::Unless => "unless",
            Keyword::Unlink => "unlink",
            Keyword::Unpack => "unpack",
            Keyword::Unshift => "unshift",
            Keyword::Untie => "untie",
            Keyword::Until => "until",
            Keyword::Use => "use",
            Keyword::Utime => "utime",
            Keyword::Values => "values",
            Keyword::Vec => "vec",
            Keyword::Wait => "wait",
            Keyword::Waitpid => "waitpid",
            Keyword::Wantarray => "wantarray",
            Keyword::Warn => "warn",
            Keyword::When => "when",
            Keyword::While => "while",
            Keyword::Write => "write",
            Keyword::X => "x",
            Keyword::Xor => "xor",
            Keyword::Y => "y",
        }
    }
}

/// Is this keyword a quote-like operator (`q`, `qq`, `qw`, `qr`, `qx`, `m`, `s`, `tr`, `y`)?  These require special
/// handling in the parser because they can start sublexing — the byte following the keyword (after whitespace) is the
/// quote delimiter, not a normal token.  The parser must avoid calling `peek_token` before deciding whether to enter
/// sublexing mode.
pub fn is_quote_keyword(kw: Keyword) -> bool {
    matches!(kw, Keyword::Q | Keyword::Qq | Keyword::Qw | Keyword::Qr | Keyword::Qx | Keyword::M | Keyword::S | Keyword::Tr | Keyword::Y)
}

/// Is this keyword a named unary operator?  After a named unary, the next thing is a term (so `/` is regex).
pub fn is_named_unary(kw: Keyword) -> bool {
    matches!(
        kw,
        Keyword::Abs
            | Keyword::Alarm
            | Keyword::Caller
            | Keyword::Chdir
            | Keyword::Chomp
            | Keyword::Chop
            | Keyword::Chr
            | Keyword::Chroot
            | Keyword::Close
            | Keyword::Closedir
            | Keyword::Cos
            | Keyword::Dbmclose
            | Keyword::Defined
            | Keyword::Delete
            | Keyword::Die
            | Keyword::Eof
            | Keyword::Evalbytes
            | Keyword::Exists
            | Keyword::Exit
            | Keyword::Exp
            | Keyword::Fc
            | Keyword::Fileno
            | Keyword::Getc
            | Keyword::Getgrgid
            | Keyword::Getgrnam
            | Keyword::Gethostbyname
            | Keyword::Getnetbyname
            | Keyword::Getpeername
            | Keyword::Getpgrp
            | Keyword::Getprotobyname
            | Keyword::Getprotobynumber
            | Keyword::Getpwnam
            | Keyword::Getpwuid
            | Keyword::Getsockname
            | Keyword::Gmtime
            | Keyword::Hex
            | Keyword::Int
            | Keyword::Lc
            | Keyword::Lcfirst
            | Keyword::Length
            | Keyword::Localtime
            | Keyword::Lock
            | Keyword::Log
            | Keyword::Oct
            | Keyword::Ord
            | Keyword::Pop
            | Keyword::Pos
            | Keyword::Prototype
            | Keyword::Quotemeta
            | Keyword::Rand
            | Keyword::Readdir
            | Keyword::Readline
            | Keyword::Readlink
            | Keyword::Readpipe
            | Keyword::Ref
            | Keyword::Require
            | Keyword::Reset
            | Keyword::Rewinddir
            | Keyword::Rmdir
            | Keyword::Scalar
            | Keyword::Sethostent
            | Keyword::Setnetent
            | Keyword::Setprotoent
            | Keyword::Setservent
            | Keyword::Shift
            | Keyword::Sin
            | Keyword::Sleep
            | Keyword::Sqrt
            | Keyword::Srand
            | Keyword::Study
            | Keyword::Telldir
            | Keyword::Tied
            | Keyword::Uc
            | Keyword::Ucfirst
            | Keyword::Umask
            | Keyword::Undef
            | Keyword::Warn
    )
}

/// Is this keyword a nullary builtin (takes no arguments)?
///
/// Nullary builtins never consume a following term as an argument, so `time+86_400` is always `time() + 86_400`.  They
/// do accept explicit empty parens: `time()`.
pub fn is_nullary(kw: Keyword) -> bool {
    matches!(
        kw,
        Keyword::Endgrent
            | Keyword::Endhostent
            | Keyword::Endnetent
            | Keyword::Endprotoent
            | Keyword::Endpwent
            | Keyword::Endservent
            | Keyword::Fork
            | Keyword::Getgrent
            | Keyword::Gethostent
            | Keyword::Getlogin
            | Keyword::Getnetent
            | Keyword::Getppid
            | Keyword::Getprotoent
            | Keyword::Getpwent
            | Keyword::Getservent
            | Keyword::Setgrent
            | Keyword::Setpwent
            | Keyword::Time
            | Keyword::Times
            | Keyword::Wait
            | Keyword::Wantarray
    )
}

/// Does this operator prefer `//` as defined-or over an empty regex argument?
///
/// Matches toke.c's `UNIDOR` macro: shift, pop, getc, pos, readline, readlink, undef, umask.  After these operators,
/// `shift // 0` is parsed as `shift() // 0`, not `shift(m//)`.
pub fn prefers_defined_or(kw: Keyword) -> bool {
    matches!(kw, Keyword::Shift | Keyword::Pop | Keyword::Getc | Keyword::Pos | Keyword::Readline | Keyword::Readlink | Keyword::Undef | Keyword::Umask)
}

/// Is this keyword a list operator?  After a list op, the next thing is a term (so `/` is regex).
pub fn is_list_op(kw: Keyword) -> bool {
    matches!(
        kw,
        Keyword::Accept
            | Keyword::Atan2
            | Keyword::Bind
            | Keyword::Binmode
            | Keyword::Bless
            | Keyword::Chmod
            | Keyword::Chown
            | Keyword::Connect
            | Keyword::Crypt
            | Keyword::Dbmopen
            | Keyword::Each
            | Keyword::Exec
            | Keyword::Fcntl
            | Keyword::Flock
            | Keyword::Formline
            | Keyword::Gethostbyaddr
            | Keyword::Getnetbyaddr
            | Keyword::Getpriority
            | Keyword::Getservbyname
            | Keyword::Getservbyport
            | Keyword::Getsockopt
            | Keyword::Glob
            | Keyword::Index
            | Keyword::Ioctl
            | Keyword::Join
            | Keyword::Keys
            | Keyword::Kill
            | Keyword::Link
            | Keyword::Listen
            | Keyword::Mkdir
            | Keyword::Msgctl
            | Keyword::Msgget
            | Keyword::Msgrcv
            | Keyword::Msgsnd
            | Keyword::Open
            | Keyword::Opendir
            | Keyword::Pack
            | Keyword::Pipe
            | Keyword::Push
            | Keyword::Read
            | Keyword::Recv
            | Keyword::Rename
            | Keyword::Reverse
            | Keyword::Rindex
            | Keyword::Seek
            | Keyword::Seekdir
            | Keyword::Select
            | Keyword::Semctl
            | Keyword::Semget
            | Keyword::Semop
            | Keyword::Send
            | Keyword::Setpgrp
            | Keyword::Setpriority
            | Keyword::Setsockopt
            | Keyword::Shmctl
            | Keyword::Shmget
            | Keyword::Shmread
            | Keyword::Shmwrite
            | Keyword::Shutdown
            | Keyword::Socket
            | Keyword::Socketpair
            | Keyword::Splice
            | Keyword::Split
            | Keyword::Sprintf
            | Keyword::Substr
            | Keyword::Symlink
            | Keyword::Syscall
            | Keyword::Sysopen
            | Keyword::Sysread
            | Keyword::Sysseek
            | Keyword::System
            | Keyword::Syswrite
            | Keyword::Tell
            | Keyword::Tie
            | Keyword::Truncate
            | Keyword::Unlink
            | Keyword::Unpack
            | Keyword::Unshift
            | Keyword::Untie
            | Keyword::Utime
            | Keyword::Values
            | Keyword::Vec
            | Keyword::Waitpid
            | Keyword::Write
    )
}

/// Is this keyword a block-list operator (sort/map/grep)?  These take an optional block as the first argument.
pub fn is_block_list_op(kw: Keyword) -> bool {
    matches!(kw, Keyword::Sort | Keyword::Map | Keyword::Grep | Keyword::Any | Keyword::All)
}

/// Is this keyword a print-like operator?  These take an optional filehandle as the first argument.
pub fn is_print_op(kw: Keyword) -> bool {
    matches!(kw, Keyword::Print | Keyword::Say | Keyword::Printf)
}

/// Keywords that have dedicated statement-level handlers in the parser.  These are consumed before dispatching, so they
/// need the fat comma check at the statement level rather than in parse_term.
pub fn is_statement_keyword(kw: Keyword) -> bool {
    matches!(
        kw,
        Keyword::ADJUST
            | Keyword::AUTOLOAD
            | Keyword::BEGIN
            | Keyword::UNITCHECK
            | Keyword::DESTROY
            | Keyword::END
            | Keyword::INIT
            | Keyword::CHECK
            | Keyword::Class
            | Keyword::Default
            | Keyword::Defer
            | Keyword::Field
            | Keyword::For
            | Keyword::Foreach
            | Keyword::Format
            | Keyword::Given
            | Keyword::If
            | Keyword::Method
            | Keyword::My
            | Keyword::No
            | Keyword::Our
            | Keyword::Package
            | Keyword::State
            | Keyword::Sub
            | Keyword::Try
            | Keyword::Unless
            | Keyword::Until
            | Keyword::Use
            | Keyword::When
            | Keyword::While
    )
}

/// Is this keyword "strong" (`+` in regen/keywords.pl)?
///
/// Strong keywords always take precedence over user-defined subs.  Weak keywords (the negation) can be overridden by
/// imported subs (via `use subs`, `Exporter`, etc.).  A local `sub abs { }` produces an "Ambiguous call resolved as
/// CORE::abs()" warning but the keyword still wins; only an imported sub actually overrides.
pub fn is_strong(kw: Keyword) -> bool {
    matches!(
        kw,
        Keyword::__DATA__
            | Keyword::__END__
            | Keyword::ADJUST
            | Keyword::AUTOLOAD
            | Keyword::BEGIN
            | Keyword::UNITCHECK
            | Keyword::DESTROY
            | Keyword::END
            | Keyword::INIT
            | Keyword::CHECK
            | Keyword::Catch
            | Keyword::Default
            | Keyword::Defer
            | Keyword::Defined
            | Keyword::Delete
            | Keyword::Do
            | Keyword::Else
            | Keyword::Elsif
            | Keyword::Elseif
            | Keyword::Eval
            | Keyword::Exists
            | Keyword::Finally
            | Keyword::For
            | Keyword::Foreach
            | Keyword::Format
            | Keyword::Given
            | Keyword::Glob
            | Keyword::Goto
            | Keyword::Grep
            | Keyword::If
            | Keyword::Last
            | Keyword::Local
            | Keyword::M
            | Keyword::Map
            | Keyword::My
            | Keyword::Next
            | Keyword::No
            | Keyword::Our
            | Keyword::Package
            | Keyword::Pos
            | Keyword::Print
            | Keyword::Printf
            | Keyword::Prototype
            | Keyword::Q
            | Keyword::Qq
            | Keyword::Qr
            | Keyword::Qw
            | Keyword::Qx
            | Keyword::Redo
            | Keyword::Require
            | Keyword::Return
            | Keyword::S
            | Keyword::Say
            | Keyword::Scalar
            | Keyword::Sort
            | Keyword::Split
            | Keyword::State
            | Keyword::Study
            | Keyword::Sub
            | Keyword::Tr
            | Keyword::Try
            | Keyword::Undef
            | Keyword::Unless
            | Keyword::Until
            | Keyword::Use
            | Keyword::When
            | Keyword::While
            | Keyword::Y
    )
}

/// Is this keyword "weak" (`-` in regen/keywords.pl)?
///
/// Weak keywords can be overridden by imported subs.  When the lexer encounters a weak keyword whose name has been
/// imported into the current package, it emits `Token::Ident` instead of `Token::Keyword`.
pub fn is_weak(kw: Keyword) -> bool {
    !is_strong(kw)
}
