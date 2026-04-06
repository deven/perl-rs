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
        "binmode" => Some(Keyword::Binmode),
        "stat" => Some(Keyword::Stat),
        "lstat" => Some(Keyword::Lstat),
        "chmod" => Some(Keyword::Chmod),
        "chown" => Some(Keyword::Chown),
        "glob" => Some(Keyword::Glob),
        "opendir" => Some(Keyword::Opendir),
        "readdir" => Some(Keyword::Readdir),
        "closedir" => Some(Keyword::Closedir),
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
            | Keyword::Stat
            | Keyword::Lstat
            | Keyword::Rmdir
            | Keyword::Chdir
            | Keyword::Close
            | Keyword::Closedir
            | Keyword::Readdir
            | Keyword::Pop
            | Keyword::Shift
            | Keyword::Wantarray
            | Keyword::Exit
            | Keyword::Tied
            | Keyword::Die
            | Keyword::Warn
            | Keyword::Undef
            | Keyword::Scalar
    )
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
            | Keyword::Printf
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
    matches!(kw, Keyword::Print | Keyword::Say)
}
