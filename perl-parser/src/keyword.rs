//! Keyword table — maps identifier strings to `Keyword` variants.

use crate::pragma::Features;
use phf::phf_map;

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

/// Perfect-hash keyword lookup table.  O(1) lookup regardless of table size.  Keys are byte slices since all keywords
/// are ASCII — this avoids unnecessary UTF-8 validation in callers that work with raw bytes.
static KEYWORDS: phf::Map<&'static [u8], Keyword> = phf_map! {
    b"__FILE__" => Keyword::__FILE__,
    b"__LINE__" => Keyword::__LINE__,
    b"__PACKAGE__" => Keyword::__PACKAGE__,
    b"__CLASS__" => Keyword::__CLASS__,
    b"__DATA__" => Keyword::__DATA__,
    b"__END__" => Keyword::__END__,
    b"__SUB__" => Keyword::__SUB__,
    b"ADJUST" => Keyword::ADJUST,
    b"AUTOLOAD" => Keyword::AUTOLOAD,
    b"BEGIN" => Keyword::BEGIN,
    b"UNITCHECK" => Keyword::UNITCHECK,
    b"DESTROY" => Keyword::DESTROY,
    b"END" => Keyword::END,
    b"INIT" => Keyword::INIT,
    b"CHECK" => Keyword::CHECK,
    b"abs" => Keyword::Abs,
    b"accept" => Keyword::Accept,
    b"alarm" => Keyword::Alarm,
    b"all" => Keyword::All,
    b"and" => Keyword::And,
    b"any" => Keyword::Any,
    b"atan2" => Keyword::Atan2,
    b"bind" => Keyword::Bind,
    b"binmode" => Keyword::Binmode,
    b"bless" => Keyword::Bless,
    b"break" => Keyword::Break,
    b"caller" => Keyword::Caller,
    b"catch" => Keyword::Catch,
    b"chdir" => Keyword::Chdir,
    b"chmod" => Keyword::Chmod,
    b"chomp" => Keyword::Chomp,
    b"chop" => Keyword::Chop,
    b"chown" => Keyword::Chown,
    b"chr" => Keyword::Chr,
    b"chroot" => Keyword::Chroot,
    b"class" => Keyword::Class,
    b"close" => Keyword::Close,
    b"closedir" => Keyword::Closedir,
    b"cmp" => Keyword::Cmp,
    b"connect" => Keyword::Connect,
    b"continue" => Keyword::Continue,
    b"cos" => Keyword::Cos,
    b"crypt" => Keyword::Crypt,
    b"dbmclose" => Keyword::Dbmclose,
    b"dbmopen" => Keyword::Dbmopen,
    b"default" => Keyword::Default,
    b"defer" => Keyword::Defer,
    b"defined" => Keyword::Defined,
    b"delete" => Keyword::Delete,
    b"die" => Keyword::Die,
    b"do" => Keyword::Do,
    b"dump" => Keyword::Dump,
    b"each" => Keyword::Each,
    b"else" => Keyword::Else,
    b"elsif" => Keyword::Elsif,
    b"elseif" => Keyword::Elseif,
    b"endgrent" => Keyword::Endgrent,
    b"endhostent" => Keyword::Endhostent,
    b"endnetent" => Keyword::Endnetent,
    b"endprotoent" => Keyword::Endprotoent,
    b"endpwent" => Keyword::Endpwent,
    b"endservent" => Keyword::Endservent,
    b"eof" => Keyword::Eof,
    b"eq" => Keyword::Eq,
    b"eval" => Keyword::Eval,
    b"evalbytes" => Keyword::Evalbytes,
    b"exec" => Keyword::Exec,
    b"exists" => Keyword::Exists,
    b"exit" => Keyword::Exit,
    b"exp" => Keyword::Exp,
    b"fc" => Keyword::Fc,
    b"fcntl" => Keyword::Fcntl,
    b"field" => Keyword::Field,
    b"fileno" => Keyword::Fileno,
    b"finally" => Keyword::Finally,
    b"flock" => Keyword::Flock,
    b"for" => Keyword::For,
    b"foreach" => Keyword::Foreach,
    b"fork" => Keyword::Fork,
    b"format" => Keyword::Format,
    b"formline" => Keyword::Formline,
    b"ge" => Keyword::Ge,
    b"getc" => Keyword::Getc,
    b"getgrent" => Keyword::Getgrent,
    b"getgrgid" => Keyword::Getgrgid,
    b"getgrnam" => Keyword::Getgrnam,
    b"gethostbyaddr" => Keyword::Gethostbyaddr,
    b"gethostbyname" => Keyword::Gethostbyname,
    b"gethostent" => Keyword::Gethostent,
    b"getlogin" => Keyword::Getlogin,
    b"getnetbyaddr" => Keyword::Getnetbyaddr,
    b"getnetbyname" => Keyword::Getnetbyname,
    b"getnetent" => Keyword::Getnetent,
    b"getpeername" => Keyword::Getpeername,
    b"getpgrp" => Keyword::Getpgrp,
    b"getppid" => Keyword::Getppid,
    b"getpriority" => Keyword::Getpriority,
    b"getprotobyname" => Keyword::Getprotobyname,
    b"getprotobynumber" => Keyword::Getprotobynumber,
    b"getprotoent" => Keyword::Getprotoent,
    b"getpwent" => Keyword::Getpwent,
    b"getpwnam" => Keyword::Getpwnam,
    b"getpwuid" => Keyword::Getpwuid,
    b"getservbyname" => Keyword::Getservbyname,
    b"getservbyport" => Keyword::Getservbyport,
    b"getservent" => Keyword::Getservent,
    b"getsockname" => Keyword::Getsockname,
    b"getsockopt" => Keyword::Getsockopt,
    b"given" => Keyword::Given,
    b"glob" => Keyword::Glob,
    b"gmtime" => Keyword::Gmtime,
    b"goto" => Keyword::Goto,
    b"grep" => Keyword::Grep,
    b"gt" => Keyword::Gt,
    b"hex" => Keyword::Hex,
    b"if" => Keyword::If,
    b"index" => Keyword::Index,
    b"int" => Keyword::Int,
    b"ioctl" => Keyword::Ioctl,
    b"isa" => Keyword::Isa,
    b"join" => Keyword::Join,
    b"keys" => Keyword::Keys,
    b"kill" => Keyword::Kill,
    b"last" => Keyword::Last,
    b"lc" => Keyword::Lc,
    b"lcfirst" => Keyword::Lcfirst,
    b"le" => Keyword::Le,
    b"length" => Keyword::Length,
    b"link" => Keyword::Link,
    b"listen" => Keyword::Listen,
    b"local" => Keyword::Local,
    b"localtime" => Keyword::Localtime,
    b"lock" => Keyword::Lock,
    b"log" => Keyword::Log,
    b"lstat" => Keyword::Lstat,
    b"lt" => Keyword::Lt,
    b"m" => Keyword::M,
    b"map" => Keyword::Map,
    b"method" => Keyword::Method,
    b"mkdir" => Keyword::Mkdir,
    b"msgctl" => Keyword::Msgctl,
    b"msgget" => Keyword::Msgget,
    b"msgrcv" => Keyword::Msgrcv,
    b"msgsnd" => Keyword::Msgsnd,
    b"my" => Keyword::My,
    b"ne" => Keyword::Ne,
    b"next" => Keyword::Next,
    b"no" => Keyword::No,
    b"not" => Keyword::Not,
    b"oct" => Keyword::Oct,
    b"open" => Keyword::Open,
    b"opendir" => Keyword::Opendir,
    b"or" => Keyword::Or,
    b"ord" => Keyword::Ord,
    b"our" => Keyword::Our,
    b"pack" => Keyword::Pack,
    b"package" => Keyword::Package,
    b"pipe" => Keyword::Pipe,
    b"pop" => Keyword::Pop,
    b"pos" => Keyword::Pos,
    b"print" => Keyword::Print,
    b"printf" => Keyword::Printf,
    b"prototype" => Keyword::Prototype,
    b"push" => Keyword::Push,
    b"q" => Keyword::Q,
    b"qq" => Keyword::Qq,
    b"qr" => Keyword::Qr,
    b"quotemeta" => Keyword::Quotemeta,
    b"qw" => Keyword::Qw,
    b"qx" => Keyword::Qx,
    b"rand" => Keyword::Rand,
    b"read" => Keyword::Read,
    b"readdir" => Keyword::Readdir,
    b"readline" => Keyword::Readline,
    b"readlink" => Keyword::Readlink,
    b"readpipe" => Keyword::Readpipe,
    b"recv" => Keyword::Recv,
    b"redo" => Keyword::Redo,
    b"ref" => Keyword::Ref,
    b"rename" => Keyword::Rename,
    b"require" => Keyword::Require,
    b"reset" => Keyword::Reset,
    b"return" => Keyword::Return,
    b"reverse" => Keyword::Reverse,
    b"rewinddir" => Keyword::Rewinddir,
    b"rindex" => Keyword::Rindex,
    b"rmdir" => Keyword::Rmdir,
    b"s" => Keyword::S,
    b"say" => Keyword::Say,
    b"scalar" => Keyword::Scalar,
    b"seek" => Keyword::Seek,
    b"seekdir" => Keyword::Seekdir,
    b"select" => Keyword::Select,
    b"semctl" => Keyword::Semctl,
    b"semget" => Keyword::Semget,
    b"semop" => Keyword::Semop,
    b"send" => Keyword::Send,
    b"setgrent" => Keyword::Setgrent,
    b"sethostent" => Keyword::Sethostent,
    b"setnetent" => Keyword::Setnetent,
    b"setpgrp" => Keyword::Setpgrp,
    b"setpriority" => Keyword::Setpriority,
    b"setprotoent" => Keyword::Setprotoent,
    b"setpwent" => Keyword::Setpwent,
    b"setservent" => Keyword::Setservent,
    b"setsockopt" => Keyword::Setsockopt,
    b"shift" => Keyword::Shift,
    b"shmctl" => Keyword::Shmctl,
    b"shmget" => Keyword::Shmget,
    b"shmread" => Keyword::Shmread,
    b"shmwrite" => Keyword::Shmwrite,
    b"shutdown" => Keyword::Shutdown,
    b"sin" => Keyword::Sin,
    b"sleep" => Keyword::Sleep,
    b"socket" => Keyword::Socket,
    b"socketpair" => Keyword::Socketpair,
    b"sort" => Keyword::Sort,
    b"splice" => Keyword::Splice,
    b"split" => Keyword::Split,
    b"sprintf" => Keyword::Sprintf,
    b"sqrt" => Keyword::Sqrt,
    b"srand" => Keyword::Srand,
    b"stat" => Keyword::Stat,
    b"state" => Keyword::State,
    b"study" => Keyword::Study,
    b"sub" => Keyword::Sub,
    b"substr" => Keyword::Substr,
    b"symlink" => Keyword::Symlink,
    b"syscall" => Keyword::Syscall,
    b"sysopen" => Keyword::Sysopen,
    b"sysread" => Keyword::Sysread,
    b"sysseek" => Keyword::Sysseek,
    b"system" => Keyword::System,
    b"syswrite" => Keyword::Syswrite,
    b"tell" => Keyword::Tell,
    b"telldir" => Keyword::Telldir,
    b"tie" => Keyword::Tie,
    b"tied" => Keyword::Tied,
    b"time" => Keyword::Time,
    b"times" => Keyword::Times,
    b"tr" => Keyword::Tr,
    b"try" => Keyword::Try,
    b"truncate" => Keyword::Truncate,
    b"uc" => Keyword::Uc,
    b"ucfirst" => Keyword::Ucfirst,
    b"umask" => Keyword::Umask,
    b"undef" => Keyword::Undef,
    b"unless" => Keyword::Unless,
    b"unlink" => Keyword::Unlink,
    b"unpack" => Keyword::Unpack,
    b"unshift" => Keyword::Unshift,
    b"untie" => Keyword::Untie,
    b"until" => Keyword::Until,
    b"use" => Keyword::Use,
    b"utime" => Keyword::Utime,
    b"values" => Keyword::Values,
    b"vec" => Keyword::Vec,
    b"wait" => Keyword::Wait,
    b"waitpid" => Keyword::Waitpid,
    b"wantarray" => Keyword::Wantarray,
    b"warn" => Keyword::Warn,
    b"when" => Keyword::When,
    b"while" => Keyword::While,
    b"write" => Keyword::Write,
    b"x" => Keyword::X,
    b"xor" => Keyword::Xor,
    b"y" => Keyword::Y,
};

/// Look up a keyword by name, respecting feature gating.  Returns `None` for unknown names and for feature-gated
/// keywords whose feature is not active (they are plain identifiers in that context).  Takes `&[u8]` since all keywords
/// are ASCII — callers working with raw bytes avoid unnecessary UTF-8 validation.
pub fn lookup_keyword(name: &[u8], features: Features) -> Option<Keyword> {
    let &kw = KEYWORDS.get(name)?;
    let needed = match kw {
        Keyword::Say => Some(Features::SAY),
        Keyword::State => Some(Features::STATE),
        Keyword::Try | Keyword::Catch | Keyword::Finally => Some(Features::TRY),
        Keyword::Defer => Some(Features::DEFER),
        Keyword::Given | Keyword::When | Keyword::Default | Keyword::Break => Some(Features::SWITCH),
        Keyword::Class | Keyword::Field | Keyword::Method | Keyword::ADJUST | Keyword::__CLASS__ => Some(Features::CLASS),
        Keyword::__SUB__ => Some(Features::CURRENT_SUB),
        Keyword::Any => Some(Features::KEYWORD_ANY),
        Keyword::All => Some(Features::KEYWORD_ALL),
        Keyword::Evalbytes => Some(Features::EVALBYTES),
        Keyword::Fc => Some(Features::FC),
        Keyword::Isa => Some(Features::ISA),
        _ => None,
    };
    if needed.is_none_or(|f| features.contains(f)) { Some(kw) } else { None }
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
