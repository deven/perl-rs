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
        "qw" => Some(Keyword::Qw),
        "format" => Some(Keyword::Format),
        "BEGIN" => Some(Keyword::BEGIN),
        "END" => Some(Keyword::END),
        "INIT" => Some(Keyword::INIT),
        "CHECK" => Some(Keyword::CHECK),
        "UNITCHECK" => Some(Keyword::UNITCHECK),
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
            Keyword::Qw => "qw",
            Keyword::Format => "format",
            Keyword::BEGIN => "BEGIN",
            Keyword::END => "END",
            Keyword::INIT => "INIT",
            Keyword::CHECK => "CHECK",
            Keyword::UNITCHECK => "UNITCHECK",
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
            | Keyword::Wantarray
            | Keyword::Exit
            | Keyword::Tied
            | Keyword::Die
            | Keyword::Warn
            | Keyword::Undef
            | Keyword::Scalar
            | Keyword::Require
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
    )
}

/// Is this keyword a block-list operator (sort/map/grep)?
/// These take an optional block as the first argument.
pub fn is_block_list_op(kw: Keyword) -> bool {
    matches!(kw, Keyword::Sort | Keyword::Map | Keyword::Grep)
}

/// Is this keyword a print-like operator?
/// These take an optional filehandle as the first argument.
pub fn is_print_op(kw: Keyword) -> bool {
    matches!(kw, Keyword::Print | Keyword::Say | Keyword::Printf)
}
