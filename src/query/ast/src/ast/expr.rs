// Copyright 2021 Datafuse Labs
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::fmt::Display;
use std::fmt::Formatter;

use databend_common_exception::ErrorCode;
use databend_common_exception::Result;
use databend_common_exception::Span;
use databend_common_functions::aggregates::AggregateFunctionFactory;
use databend_common_io::display_decimal_256;
use databend_common_io::escape_string_with_quote;
use enum_as_inner::EnumAsInner;
use ethnum::i256;

use super::OrderByExpr;
use crate::ast::write_comma_separated_list;
use crate::ast::write_dot_separated_list;
use crate::ast::ColumnPosition;
use crate::ast::Identifier;
use crate::ast::Query;

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum IntervalKind {
    Year,
    Quarter,
    Month,
    Day,
    Hour,
    Minute,
    Second,
    Doy,
    Week,
    Dow,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ColumnID {
    Name(Identifier),
    Position(ColumnPosition),
}

impl ColumnID {
    pub fn name(&self) -> &str {
        match self {
            ColumnID::Name(id) => &id.name,
            ColumnID::Position(id) => &id.name,
        }
    }
}

impl Display for ColumnID {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            ColumnID::Name(id) => write!(f, "{}", id),
            ColumnID::Position(id) => write!(f, "{}", id),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    /// Column reference, with indirection like `table.column`
    ColumnRef {
        span: Span,
        database: Option<Identifier>,
        table: Option<Identifier>,
        column: ColumnID,
    },
    /// `IS [ NOT ] NULL` expression
    IsNull {
        span: Span,
        expr: Box<Expr>,
        not: bool,
    },
    /// `IS [NOT] DISTINCT` expression
    IsDistinctFrom {
        span: Span,
        left: Box<Expr>,
        right: Box<Expr>,
        not: bool,
    },
    /// `[ NOT ] IN (expr, ...)`
    InList {
        span: Span,
        expr: Box<Expr>,
        list: Vec<Expr>,
        not: bool,
    },
    /// `[ NOT ] IN (SELECT ...)`
    InSubquery {
        span: Span,
        expr: Box<Expr>,
        subquery: Box<Query>,
        not: bool,
    },
    /// `BETWEEN ... AND ...`
    Between {
        span: Span,
        expr: Box<Expr>,
        low: Box<Expr>,
        high: Box<Expr>,
        not: bool,
    },
    /// Binary operation
    BinaryOp {
        span: Span,
        op: BinaryOperator,
        left: Box<Expr>,
        right: Box<Expr>,
    },
    /// JSON operation
    JsonOp {
        span: Span,
        op: JsonOperator,
        left: Box<Expr>,
        right: Box<Expr>,
    },
    /// Unary operation
    UnaryOp {
        span: Span,
        op: UnaryOperator,
        expr: Box<Expr>,
    },
    /// `CAST` expression, like `CAST(expr AS target_type)`
    Cast {
        span: Span,
        expr: Box<Expr>,
        target_type: TypeName,
        pg_style: bool,
    },
    /// `TRY_CAST` expression`
    TryCast {
        span: Span,
        expr: Box<Expr>,
        target_type: TypeName,
    },
    /// EXTRACT(IntervalKind FROM <expr>)
    Extract {
        span: Span,
        kind: IntervalKind,
        expr: Box<Expr>,
    },
    /// DATE_PART(IntervalKind, <expr>)
    DatePart {
        span: Span,
        kind: IntervalKind,
        expr: Box<Expr>,
    },
    /// POSITION(<expr> IN <expr>)
    Position {
        span: Span,
        substr_expr: Box<Expr>,
        str_expr: Box<Expr>,
    },
    /// SUBSTRING(<expr> [FROM <expr>] [FOR <expr>])
    Substring {
        span: Span,
        expr: Box<Expr>,
        substring_from: Box<Expr>,
        substring_for: Option<Box<Expr>>,
    },
    /// TRIM([[BOTH | LEADING | TRAILING] <expr> FROM] <expr>)
    /// Or
    /// TRIM(<expr>)
    Trim {
        span: Span,
        expr: Box<Expr>,
        // ([BOTH | LEADING | TRAILING], <expr>)
        trim_where: Option<(TrimWhere, Box<Expr>)>,
    },
    /// A literal value, such as string, number, date or NULL
    Literal { span: Span, lit: Literal },
    /// `COUNT(*)` expression
    CountAll { span: Span, window: Option<Window> },
    /// `(foo, bar)`
    Tuple { span: Span, exprs: Vec<Expr> },
    /// Scalar/Agg/Window function call
    FunctionCall {
        span: Span,
        /// Set to true if the function is aggregate function with `DISTINCT`, like `COUNT(DISTINCT a)`
        distinct: bool,
        name: Identifier,
        args: Vec<Expr>,
        params: Vec<Expr>,
        window: Option<Window>,
        lambda: Option<Lambda>,
    },
    /// `CASE ... WHEN ... ELSE ...` expression
    Case {
        span: Span,
        operand: Option<Box<Expr>>,
        conditions: Vec<Expr>,
        results: Vec<Expr>,
        else_result: Option<Box<Expr>>,
    },
    /// `EXISTS` expression
    Exists {
        span: Span,
        /// Indicate if this is a `NOT EXISTS`
        not: bool,
        subquery: Box<Query>,
    },
    /// Scalar/ANY/ALL/SOME subquery
    Subquery {
        span: Span,
        modifier: Option<SubqueryModifier>,
        subquery: Box<Query>,
    },
    /// Access elements of `Array`, `Map` and `Variant` by index or key, like `arr[0]`, or `obj:k1`
    MapAccess {
        span: Span,
        expr: Box<Expr>,
        accessor: MapAccessor,
    },
    /// The `Array` expr
    Array { span: Span, exprs: Vec<Expr> },
    /// The `Map` expr
    Map {
        span: Span,
        kvs: Vec<(Literal, Expr)>,
    },
    /// The `Interval 1 DAY` expr
    Interval {
        span: Span,
        expr: Box<Expr>,
        unit: IntervalKind,
    },
    DateAdd {
        span: Span,
        unit: IntervalKind,
        interval: Box<Expr>,
        date: Box<Expr>,
    },
    DateSub {
        span: Span,
        unit: IntervalKind,
        interval: Box<Expr>,
        date: Box<Expr>,
    },
    DateTrunc {
        span: Span,
        unit: IntervalKind,
        date: Box<Expr>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubqueryModifier {
    Any,
    All,
    Some,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Literal {
    UInt64(u64),
    Float64(f64),
    Decimal256 {
        value: i256,
        precision: u8,
        scale: u8,
    },
    // Quoted string literal value
    String(String),
    Boolean(bool),
    Null,
}

impl Literal {}

/// The display style for a map access expression
#[derive(Debug, Clone, PartialEq)]
pub enum MapAccessor {
    /// `[0][1]`
    Bracket { key: Box<Expr> },
    /// `.1`
    DotNumber { key: u64 },
    /// `:a:b`
    Colon { key: Identifier },
}

#[derive(Debug, Clone, PartialEq)]
pub enum TypeName {
    Boolean,
    UInt8,
    UInt16,
    UInt32,
    UInt64,
    Int8,
    Int16,
    Int32,
    Int64,
    Float32,
    Float64,
    Decimal {
        precision: u8,
        scale: u8,
    },
    Date,
    Timestamp,
    Binary,
    String,
    Array(Box<TypeName>),
    Map {
        key_type: Box<TypeName>,
        val_type: Box<TypeName>,
    },
    Bitmap,
    Tuple {
        fields_name: Option<Vec<String>>,
        fields_type: Vec<TypeName>,
    },
    Variant,
    Nullable(Box<TypeName>),
    NotNull(Box<TypeName>),
}

impl TypeName {
    pub fn is_nullable(&self) -> bool {
        matches!(self, TypeName::Nullable(_))
    }

    pub fn wrap_nullable(self) -> Self {
        if !self.is_nullable() {
            Self::Nullable(Box::new(self))
        } else {
            self
        }
    }

    pub fn wrap_not_null(self) -> Self {
        match self {
            Self::NotNull(_) => self,
            _ => Self::NotNull(Box::new(self)),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrimWhere {
    Both,
    Leading,
    Trailing,
}

#[derive(Debug, Clone, PartialEq, EnumAsInner)]
pub enum Window {
    WindowReference(WindowRef),
    WindowSpec(WindowSpec),
}

#[derive(Debug, Clone, PartialEq)]
pub struct WindowDefinition {
    pub name: Identifier,
    pub spec: WindowSpec,
}

#[derive(Debug, Clone, PartialEq)]
pub struct WindowRef {
    pub window_name: Identifier,
}

#[derive(Debug, Clone, PartialEq)]
pub struct WindowSpec {
    pub existing_window_name: Option<Identifier>,
    pub partition_by: Vec<Expr>,
    pub order_by: Vec<OrderByExpr>,
    pub window_frame: Option<WindowFrame>,
}

/// `RANGE UNBOUNDED PRECEDING` or `ROWS BETWEEN 5 PRECEDING AND CURRENT ROW`.
#[derive(Debug, Clone, PartialEq)]
pub struct WindowFrame {
    pub units: WindowFrameUnits,
    pub start_bound: WindowFrameBound,
    pub end_bound: WindowFrameBound,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, EnumAsInner)]
pub enum WindowFrameUnits {
    Rows,
    Range,
}

/// Specifies [WindowFrame]'s `start_bound` and `end_bound`
#[derive(Debug, Clone, PartialEq)]
pub enum WindowFrameBound {
    /// `CURRENT ROW`
    CurrentRow,
    /// `<N> PRECEDING` or `UNBOUNDED PRECEDING`
    Preceding(Option<Box<Expr>>),
    /// `<N> FOLLOWING` or `UNBOUNDED FOLLOWING`.
    Following(Option<Box<Expr>>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Lambda {
    pub params: Vec<Identifier>,
    pub expr: Box<Expr>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BinaryOperator {
    Plus,
    Minus,
    Multiply,
    Div,
    Divide,
    IntDiv,
    Modulo,
    StringConcat,
    // `>` operator
    Gt,
    // `<` operator
    Lt,
    // `>=` operator
    Gte,
    // `<=` operator
    Lte,
    Eq,
    NotEq,
    Caret,
    And,
    Or,
    Xor,
    Like,
    NotLike,
    Regexp,
    RLike,
    NotRegexp,
    NotRLike,
    SoundsLike,
    BitwiseOr,
    BitwiseAnd,
    BitwiseXor,
    BitwiseShiftLeft,
    BitwiseShiftRight,
    L2Distance,
}

impl BinaryOperator {
    pub fn to_contrary(&self) -> Result<Self> {
        match &self {
            BinaryOperator::Gt => Ok(BinaryOperator::Lte),
            BinaryOperator::Lt => Ok(BinaryOperator::Gte),
            BinaryOperator::Gte => Ok(BinaryOperator::Lt),
            BinaryOperator::Lte => Ok(BinaryOperator::Gt),
            BinaryOperator::Eq => Ok(BinaryOperator::NotEq),
            BinaryOperator::NotEq => Ok(BinaryOperator::Eq),
            _ => Err(ErrorCode::Unimplemented(format!(
                "Converting {self} to its contrary is not currently supported"
            ))),
        }
    }

    pub fn to_func_name(&self) -> String {
        match self {
            BinaryOperator::StringConcat => "concat".to_string(),
            BinaryOperator::BitwiseOr => "bit_or".to_string(),
            BinaryOperator::BitwiseAnd => "bit_and".to_string(),
            BinaryOperator::BitwiseXor => "bit_xor".to_string(),
            BinaryOperator::BitwiseShiftLeft => "bit_shift_left".to_string(),
            BinaryOperator::BitwiseShiftRight => "bit_shift_right".to_string(),
            BinaryOperator::Caret => "pow".to_string(),
            BinaryOperator::L2Distance => "l2_distance".to_string(),
            _ => {
                let name = format!("{:?}", self);
                name.to_lowercase()
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JsonOperator {
    /// -> keeps the value as json
    Arrow,
    /// ->> keeps the value as text or int.
    LongArrow,
    /// #> Extracts JSON sub-object at the specified path
    HashArrow,
    /// #>> Extracts JSON sub-object at the specified path as text
    HashLongArrow,
    /// ? Checks whether text key exist as top-level key or array element.
    Question,
    /// ?| Checks whether any of the text keys exist as top-level keys or array elements.
    QuestionOr,
    /// ?& Checks whether all of the text keys exist as top-level keys or array elements.
    QuestionAnd,
    /// @> Checks whether left json contains the right json
    AtArrow,
    /// <@ Checks whether right json contains the left json
    ArrowAt,
    /// @? Checks whether JSON path return any item for the specified JSON value
    AtQuestion,
    /// @@ Returns the result of a JSON path predicate check for the specified JSON value.
    AtAt,
}

impl JsonOperator {
    pub fn to_func_name(&self) -> String {
        match self {
            JsonOperator::Arrow => "get".to_string(),
            JsonOperator::LongArrow => "get_string".to_string(),
            JsonOperator::HashArrow => "get_by_keypath".to_string(),
            JsonOperator::HashLongArrow => "get_by_keypath_string".to_string(),
            JsonOperator::Question => "json_exists_key".to_string(),
            JsonOperator::QuestionOr => "json_exists_any_keys".to_string(),
            JsonOperator::QuestionAnd => "json_exists_all_keys".to_string(),
            JsonOperator::AtArrow => "json_contains_in_left".to_string(),
            JsonOperator::ArrowAt => "json_contains_in_right".to_string(),
            JsonOperator::AtQuestion => "json_path_exists".to_string(),
            JsonOperator::AtAt => "json_path_match".to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnaryOperator {
    Plus,
    Minus,
    Not,
    Factorial,
    SquareRoot,
    CubeRoot,
    Abs,
    BitwiseNot,
}

impl UnaryOperator {
    pub fn to_func_name(&self) -> String {
        match self {
            UnaryOperator::SquareRoot => "sqrt".to_string(),
            UnaryOperator::CubeRoot => "cbrt".to_string(),
            UnaryOperator::BitwiseNot => "bit_not".to_string(),
            _ => {
                let name = format!("{:?}", self);
                name.to_lowercase()
            }
        }
    }
}

impl Expr {
    pub fn span(&self) -> Span {
        match self {
            Expr::ColumnRef { span, .. }
            | Expr::IsNull { span, .. }
            | Expr::IsDistinctFrom { span, .. }
            | Expr::InList { span, .. }
            | Expr::InSubquery { span, .. }
            | Expr::Between { span, .. }
            | Expr::BinaryOp { span, .. }
            | Expr::JsonOp { span, .. }
            | Expr::UnaryOp { span, .. }
            | Expr::Cast { span, .. }
            | Expr::TryCast { span, .. }
            | Expr::Extract { span, .. }
            | Expr::DatePart { span, .. }
            | Expr::Position { span, .. }
            | Expr::Substring { span, .. }
            | Expr::Trim { span, .. }
            | Expr::Literal { span, .. }
            | Expr::CountAll { span, .. }
            | Expr::Tuple { span, .. }
            | Expr::FunctionCall { span, .. }
            | Expr::Case { span, .. }
            | Expr::Exists { span, .. }
            | Expr::Subquery { span, .. }
            | Expr::MapAccess { span, .. }
            | Expr::Array { span, .. }
            | Expr::Map { span, .. }
            | Expr::Interval { span, .. }
            | Expr::DateAdd { span, .. }
            | Expr::DateSub { span, .. }
            | Expr::DateTrunc { span, .. } => *span,
        }
    }

    pub fn all_function_like_syntaxes() -> &'static [&'static str] {
        &[
            "CAST",
            "TRY_CAST",
            "EXTRACT",
            "DATE_PART",
            "POSITION",
            "SUBSTRING",
            "TRIM",
            "DATE_ADD",
            "DATE_SUB",
            "DATE_TRUNC",
        ]
    }
}

impl Display for IntervalKind {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str(match self {
            IntervalKind::Year => "YEAR",
            IntervalKind::Quarter => "QUARTER",
            IntervalKind::Month => "MONTH",
            IntervalKind::Day => "DAY",
            IntervalKind::Hour => "HOUR",
            IntervalKind::Minute => "MINUTE",
            IntervalKind::Second => "SECOND",
            IntervalKind::Doy => "DOY",
            IntervalKind::Dow => "DOW",
            IntervalKind::Week => "WEEK",
        })
    }
}

impl Display for SubqueryModifier {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            SubqueryModifier::Any => write!(f, "ANY"),
            SubqueryModifier::All => write!(f, "ALL"),
            SubqueryModifier::Some => write!(f, "SOME"),
        }
    }
}

impl Display for UnaryOperator {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            UnaryOperator::Plus => {
                write!(f, "+")
            }
            UnaryOperator::Minus => {
                write!(f, "-")
            }
            UnaryOperator::Not => {
                write!(f, "NOT")
            }
            UnaryOperator::SquareRoot => {
                write!(f, "|/")
            }
            UnaryOperator::CubeRoot => {
                write!(f, "||/")
            }
            UnaryOperator::Factorial => {
                write!(f, "!")
            }
            UnaryOperator::Abs => {
                write!(f, "@")
            }
            UnaryOperator::BitwiseNot => {
                write!(f, "~")
            }
        }
    }
}

impl Display for BinaryOperator {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            BinaryOperator::Plus => {
                write!(f, "+")
            }
            BinaryOperator::Minus => {
                write!(f, "-")
            }
            BinaryOperator::Multiply => {
                write!(f, "*")
            }
            BinaryOperator::Div => {
                write!(f, "DIV")
            }
            BinaryOperator::Divide => {
                write!(f, "/")
            }
            BinaryOperator::IntDiv => {
                write!(f, "//")
            }
            BinaryOperator::Modulo => {
                write!(f, "%")
            }
            BinaryOperator::StringConcat => {
                write!(f, "||")
            }
            BinaryOperator::Gt => {
                write!(f, ">")
            }
            BinaryOperator::Lt => {
                write!(f, "<")
            }
            BinaryOperator::Gte => {
                write!(f, ">=")
            }
            BinaryOperator::Lte => {
                write!(f, "<=")
            }
            BinaryOperator::Eq => {
                write!(f, "=")
            }
            BinaryOperator::NotEq => {
                write!(f, "<>")
            }
            BinaryOperator::Caret => {
                write!(f, "^")
            }
            BinaryOperator::And => {
                write!(f, "AND")
            }
            BinaryOperator::Or => {
                write!(f, "OR")
            }
            BinaryOperator::Xor => {
                write!(f, "XOR")
            }
            BinaryOperator::Like => {
                write!(f, "LIKE")
            }
            BinaryOperator::NotLike => {
                write!(f, "NOT LIKE")
            }
            BinaryOperator::Regexp => {
                write!(f, "REGEXP")
            }
            BinaryOperator::RLike => {
                write!(f, "RLIKE")
            }
            BinaryOperator::NotRegexp => {
                write!(f, "NOT REGEXP")
            }
            BinaryOperator::NotRLike => {
                write!(f, "NOT RLIKE")
            }
            BinaryOperator::SoundsLike => {
                write!(f, "SOUNDS LIKE")
            }
            BinaryOperator::BitwiseOr => {
                write!(f, "|")
            }
            BinaryOperator::BitwiseAnd => {
                write!(f, "&")
            }
            BinaryOperator::BitwiseXor => {
                write!(f, "#")
            }
            BinaryOperator::BitwiseShiftLeft => {
                write!(f, "<<")
            }
            BinaryOperator::BitwiseShiftRight => {
                write!(f, ">>")
            }
            BinaryOperator::L2Distance => {
                write!(f, "<->")
            }
        }
    }
}

impl Display for JsonOperator {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            JsonOperator::Arrow => {
                write!(f, "->")
            }
            JsonOperator::LongArrow => {
                write!(f, "->>")
            }
            JsonOperator::HashArrow => {
                write!(f, "#>")
            }
            JsonOperator::HashLongArrow => {
                write!(f, "#>>")
            }
            JsonOperator::Question => {
                write!(f, "?")
            }
            JsonOperator::QuestionOr => {
                write!(f, "?|")
            }
            JsonOperator::QuestionAnd => {
                write!(f, "?&")
            }
            JsonOperator::AtArrow => {
                write!(f, "@>")
            }
            JsonOperator::ArrowAt => {
                write!(f, "<@")
            }
            JsonOperator::AtQuestion => {
                write!(f, "@?")
            }
            JsonOperator::AtAt => {
                write!(f, "@@")
            }
        }
    }
}

impl Display for TypeName {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            TypeName::Boolean => {
                write!(f, "BOOLEAN")?;
            }
            TypeName::UInt8 => {
                write!(f, "UInt8")?;
            }
            TypeName::UInt16 => {
                write!(f, "UInt16")?;
            }
            TypeName::UInt32 => {
                write!(f, "UInt32")?;
            }
            TypeName::UInt64 => {
                write!(f, "UInt64")?;
            }
            TypeName::Int8 => {
                write!(f, "Int8")?;
            }
            TypeName::Int16 => {
                write!(f, "Int16")?;
            }
            TypeName::Int32 => {
                write!(f, "Int32")?;
            }
            TypeName::Int64 => {
                write!(f, "Int64")?;
            }
            TypeName::Float32 => {
                write!(f, "Float32")?;
            }
            TypeName::Float64 => {
                write!(f, "Float64")?;
            }
            TypeName::Decimal { precision, scale } => {
                write!(f, "Decimal({}, {})", precision, scale)?;
            }
            TypeName::Date => {
                write!(f, "DATE")?;
            }
            TypeName::Timestamp => {
                write!(f, "TIMESTAMP")?;
            }
            TypeName::Binary => {
                write!(f, "BINARY")?;
            }
            TypeName::String => {
                write!(f, "STRING")?;
            }
            TypeName::Array(ty) => {
                write!(f, "ARRAY({})", ty)?;
            }
            TypeName::Map { key_type, val_type } => {
                write!(f, "MAP({}, {})", key_type, val_type)?;
            }
            TypeName::Bitmap => {
                write!(f, "BITMAP")?;
            }
            TypeName::Tuple {
                fields_name,
                fields_type,
            } => {
                write!(f, "TUPLE(")?;
                let mut first = true;
                match fields_name {
                    Some(fields_name) => {
                        for (name, ty) in fields_name.iter().zip(fields_type.iter()) {
                            if !first {
                                write!(f, ", ")?;
                            }
                            first = false;
                            write!(f, "{} {}", name, ty)?;
                        }
                    }
                    None => {
                        for ty in fields_type.iter() {
                            if !first {
                                write!(f, ", ")?;
                            }
                            first = false;
                            write!(f, "{}", ty)?;
                        }
                    }
                }
                write!(f, ")")?;
            }
            TypeName::Variant => {
                write!(f, "VARIANT")?;
            }
            TypeName::Nullable(ty) => {
                write!(f, "{} NULL", ty)?;
            }
            TypeName::NotNull(ty) => {
                write!(f, "{} NOT NULL", ty)?;
            }
        }
        Ok(())
    }
}

impl Display for TrimWhere {
    fn fmt(&self, f: &mut Formatter) -> std::fmt::Result {
        f.write_str(match self {
            TrimWhere::Both => "BOTH",
            TrimWhere::Leading => "LEADING",
            TrimWhere::Trailing => "TRAILING",
        })
    }
}

impl Display for Literal {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Literal::UInt64(val) => {
                write!(f, "{val}")
            }
            Literal::Decimal256 { value, scale, .. } => {
                write!(f, "{}", display_decimal_256(*value, *scale))
            }
            Literal::Float64(val) => {
                write!(f, "{val}")
            }
            Literal::String(val) => {
                write!(f, "\'{}\'", escape_string_with_quote(val, Some('\'')))
            }
            Literal::Boolean(val) => {
                if *val {
                    write!(f, "TRUE")
                } else {
                    write!(f, "FALSE")
                }
            }
            Literal::Null => {
                write!(f, "NULL")
            }
        }
    }
}

impl Display for WindowDefinition {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "WINDOW {} {}", self.name, self.spec)
    }
}

impl Display for Window {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let window_fmt = match *self {
            Window::WindowSpec(ref window_spec) => format!("{}", window_spec),
            Window::WindowReference(ref window_ref) => format!("{}", window_ref),
        };
        write!(f, "{}", window_fmt)
    }
}

impl Display for WindowRef {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "WINDOW {}", self.window_name)
    }
}

impl Display for WindowSpec {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let mut first = true;
        if !self.partition_by.is_empty() {
            first = false;
            write!(f, "PARTITION BY ")?;
            for (i, p) in self.partition_by.iter().enumerate() {
                if i > 0 {
                    write!(f, ", ")?;
                }
                write!(f, "{p}")?;
            }
        }

        if !self.order_by.is_empty() {
            if !first {
                write!(f, " ")?;
            }
            first = false;
            write!(f, "ORDER BY ")?;
            for (i, o) in self.order_by.iter().enumerate() {
                if i > 0 {
                    write!(f, ", ")?;
                }
                write!(f, "{o}")?;
            }
        }

        if let Some(frame) = &self.window_frame {
            if !first {
                write!(f, " ")?;
            }
            match frame.units {
                WindowFrameUnits::Rows => {
                    write!(f, "ROWS")?;
                }
                WindowFrameUnits::Range => {
                    write!(f, "RANGE")?;
                }
            }

            let format_frame = |frame: &WindowFrameBound| -> String {
                match frame {
                    WindowFrameBound::CurrentRow => "CURRENT ROW".to_string(),
                    WindowFrameBound::Preceding(None) => "UNBOUNDED PRECEDING".to_string(),
                    WindowFrameBound::Following(None) => "UNBOUNDED FOLLOWING".to_string(),
                    WindowFrameBound::Preceding(Some(n)) => format!("{} PRECEDING", n),
                    WindowFrameBound::Following(Some(n)) => format!("{} FOLLOWING", n),
                }
            };
            write!(
                f,
                " BETWEEN {} AND {}",
                format_frame(&frame.start_bound),
                format_frame(&frame.end_bound)
            )?
        }
        Ok(())
    }
}

impl Display for Lambda {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        if self.params.len() == 1 {
            write!(f, "{}", self.params[0])?;
        } else {
            write!(f, "(")?;
            write_comma_separated_list(f, self.params.clone())?;
            write!(f, ")")?;
        }
        write!(f, " -> {}", self.expr)?;

        Ok(())
    }
}

impl Display for Expr {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Expr::ColumnRef {
                database,
                table,
                column,
                ..
            } => {
                if f.alternate() {
                    write!(f, "{}", column)?;
                } else {
                    write_dot_separated_list(f, database.iter().chain(table))?;
                    if table.is_some() {
                        write!(f, ".")?;
                    }
                    write!(f, "{}", column)?;
                }
            }
            Expr::IsNull { expr, not, .. } => {
                write!(f, "{expr} IS")?;
                if *not {
                    write!(f, " NOT")?;
                }
                write!(f, " NULL")?;
            }
            Expr::IsDistinctFrom {
                left, right, not, ..
            } => {
                write!(f, "{left} IS")?;
                if *not {
                    write!(f, " NOT")?;
                }
                write!(f, " DISTINCT FROM {right}")?;
            }

            Expr::InList {
                expr, list, not, ..
            } => {
                write!(f, "{expr}")?;
                if *not {
                    write!(f, " NOT")?;
                }
                write!(f, " IN(")?;
                write_comma_separated_list(f, list)?;
                write!(f, ")")?;
            }
            Expr::InSubquery {
                expr,
                subquery,
                not,
                ..
            } => {
                write!(f, "{expr}")?;
                if *not {
                    write!(f, " NOT")?;
                }
                write!(f, " IN({subquery})")?;
            }
            Expr::Between {
                expr,
                low,
                high,
                not,
                ..
            } => {
                write!(f, "{expr}")?;
                if *not {
                    write!(f, " NOT")?;
                }
                write!(f, " BETWEEN {low} AND {high}")?;
            }
            Expr::UnaryOp { op, expr, .. } => {
                match op {
                    // TODO (xieqijun) Maybe special attribute are provided to check whether the symbol is before or after.
                    UnaryOperator::Factorial => {
                        write!(f, "({expr} {op})")?;
                    }
                    _ => {
                        write!(f, "({op} {expr})")?;
                    }
                }
            }
            Expr::BinaryOp {
                op, left, right, ..
            } => {
                write!(f, "({left} {op} {right})")?;
            }
            Expr::JsonOp {
                op, left, right, ..
            } => {
                write!(f, "({left} {op} {right})")?;
            }
            Expr::Cast {
                expr,
                target_type,
                pg_style,
                ..
            } => {
                if *pg_style {
                    write!(f, "{expr}::{target_type}")?;
                } else {
                    write!(f, "CAST({expr} AS {target_type})")?;
                }
            }
            Expr::TryCast {
                expr, target_type, ..
            } => {
                write!(f, "TRY_CAST({expr} AS {target_type})")?;
            }
            Expr::Extract {
                kind: field, expr, ..
            } => {
                write!(f, "EXTRACT({field} FROM {expr})")?;
            }
            Expr::DatePart {
                kind: field, expr, ..
            } => {
                write!(f, "DATE_PART({field}, {expr})")?;
            }
            Expr::Position {
                substr_expr,
                str_expr,
                ..
            } => {
                write!(f, "POSITION({substr_expr} IN {str_expr})")?;
            }
            Expr::Substring {
                expr,
                substring_from,
                substring_for,
                ..
            } => {
                write!(f, "SUBSTRING({expr} FROM {substring_from}")?;
                if let Some(substring_for) = substring_for {
                    write!(f, " FOR {substring_for}")?;
                }
                write!(f, ")")?;
            }
            Expr::Trim {
                expr, trim_where, ..
            } => {
                write!(f, "TRIM(")?;
                if let Some((trim_where, trim_str)) = trim_where {
                    write!(f, "{trim_where} {trim_str} FROM ")?;
                }
                write!(f, "{expr})")?;
            }
            Expr::Literal { lit, .. } => {
                write!(f, "{lit}")?;
            }
            Expr::CountAll { window, .. } => {
                write!(f, "COUNT(*)")?;
                if let Some(window) = window {
                    write!(f, " OVER ({window})")?;
                }
            }
            Expr::Tuple { exprs, .. } => {
                write!(f, "(")?;
                write_comma_separated_list(f, exprs)?;
                if exprs.len() == 1 {
                    write!(f, ",")?;
                }
                write!(f, ")")?;
            }
            Expr::FunctionCall {
                distinct,
                name,
                args,
                params,
                window,
                lambda,
                ..
            } => {
                write!(f, "{name}")?;
                if !params.is_empty() {
                    write!(f, "(")?;
                    write_comma_separated_list(f, params)?;
                    write!(f, ")")?;
                }
                write!(f, "(")?;
                if *distinct {
                    write!(f, "DISTINCT ")?;
                }
                write_comma_separated_list(f, args)?;
                if let Some(lambda) = lambda {
                    write!(f, ", {lambda}")?;
                }
                write!(f, ")")?;

                if let Some(window) = window {
                    write!(f, " OVER ({window})")?;
                }
            }
            Expr::Case {
                operand,
                conditions,
                results,
                else_result,
                ..
            } => {
                write!(f, "CASE")?;
                if let Some(op) = operand {
                    write!(f, " {op} ")?;
                }
                for (cond, res) in conditions.iter().zip(results) {
                    write!(f, " WHEN {cond} THEN {res}")?;
                }
                if let Some(el) = else_result {
                    write!(f, " ELSE {el}")?;
                }
                write!(f, " END")?;
            }
            Expr::Exists { not, subquery, .. } => {
                if *not {
                    write!(f, "NOT ")?;
                }
                write!(f, "EXISTS ({subquery})")?;
            }
            Expr::Subquery {
                subquery, modifier, ..
            } => {
                if let Some(m) = modifier {
                    write!(f, "{m} ")?;
                }
                write!(f, "({subquery})")?;
            }
            Expr::MapAccess { expr, accessor, .. } => {
                write!(f, "{}", expr)?;
                match accessor {
                    MapAccessor::Bracket { key } => write!(f, "[{key}]")?,
                    MapAccessor::DotNumber { key } => write!(f, ".{key}")?,
                    MapAccessor::Colon { key } => write!(f, ":{key}")?,
                }
            }
            Expr::Array { exprs, .. } => {
                write!(f, "[")?;
                write_comma_separated_list(f, exprs)?;
                write!(f, "]")?;
            }
            Expr::Map { kvs, .. } => {
                write!(f, "{{")?;
                for (i, (k, v)) in kvs.iter().enumerate() {
                    if i > 0 {
                        write!(f, ",")?;
                    }
                    write!(f, "{k}:{v}")?;
                }
                write!(f, "}}")?;
            }
            Expr::Interval { expr, unit, .. } => {
                write!(f, "INTERVAL {expr} {unit}")?;
            }
            Expr::DateAdd {
                unit,
                interval,
                date,
                ..
            } => {
                write!(f, "DATE_ADD({unit}, {interval}, {date})")?;
            }
            Expr::DateSub {
                unit,
                interval,
                date,
                ..
            } => {
                write!(f, "DATE_SUB({unit}, {interval}, {date})")?;
            }
            Expr::DateTrunc { unit, date, .. } => {
                write!(f, "DATE_TRUNC({unit}, {date})")?;
            }
        }

        Ok(())
    }
}

pub fn split_conjunctions_expr(expr: &Expr) -> Vec<Expr> {
    match expr {
        Expr::BinaryOp {
            op, left, right, ..
        } if op == &BinaryOperator::And => {
            let mut result = split_conjunctions_expr(left);
            result.extend(split_conjunctions_expr(right));
            result
        }
        _ => vec![expr.clone()],
    }
}

pub fn split_equivalent_predicate_expr(expr: &Expr) -> Option<(Expr, Expr)> {
    match expr {
        Expr::BinaryOp {
            op, left, right, ..
        } if op == &BinaryOperator::Eq => Some((*left.clone(), *right.clone())),
        _ => None,
    }
}

// If contain agg function in Expr
pub fn contain_agg_func(expr: &Expr) -> bool {
    match expr {
        Expr::ColumnRef { .. } => false,
        Expr::IsNull { expr, .. } => contain_agg_func(expr),
        Expr::IsDistinctFrom { left, right, .. } => {
            contain_agg_func(left) || contain_agg_func(right)
        }
        Expr::InList { expr, list, .. } => {
            contain_agg_func(expr) || list.iter().any(contain_agg_func)
        }
        Expr::InSubquery { expr, .. } => contain_agg_func(expr),
        Expr::Between {
            expr, low, high, ..
        } => contain_agg_func(expr) || contain_agg_func(low) || contain_agg_func(high),
        Expr::BinaryOp { left, right, .. } => contain_agg_func(left) || contain_agg_func(right),
        Expr::JsonOp { left, right, .. } => contain_agg_func(left) || contain_agg_func(right),
        Expr::UnaryOp { expr, .. } => contain_agg_func(expr),
        Expr::Cast { expr, .. } => contain_agg_func(expr),
        Expr::TryCast { expr, .. } => contain_agg_func(expr),
        Expr::Extract { expr, .. } => contain_agg_func(expr),
        Expr::DatePart { expr, .. } => contain_agg_func(expr),
        Expr::Position {
            substr_expr,
            str_expr,
            ..
        } => contain_agg_func(substr_expr) || contain_agg_func(str_expr),
        Expr::Substring {
            expr,
            substring_for,
            substring_from,
            ..
        } => {
            if let Some(substring_for) = substring_for {
                contain_agg_func(expr) || contain_agg_func(substring_for)
            } else {
                contain_agg_func(expr) || contain_agg_func(substring_from)
            }
        }
        Expr::Trim { expr, .. } => contain_agg_func(expr),
        Expr::Literal { .. } => false,
        Expr::CountAll { .. } => false,
        Expr::Tuple { exprs, .. } => exprs.iter().any(contain_agg_func),
        Expr::FunctionCall { name, .. } => {
            AggregateFunctionFactory::instance().contains(name.to_string())
        }
        Expr::Case {
            operand,
            conditions,
            results,
            else_result,
            ..
        } => {
            if let Some(operand) = operand {
                if contain_agg_func(operand) {
                    return true;
                }
            }
            if conditions.iter().any(contain_agg_func) {
                return true;
            }
            if results.iter().any(contain_agg_func) {
                return true;
            }
            if let Some(else_result) = else_result {
                if contain_agg_func(else_result) {
                    return true;
                }
            }
            false
        }
        Expr::Exists { .. } => false,
        Expr::Subquery { .. } => false,
        Expr::MapAccess { expr, .. } => contain_agg_func(expr),
        Expr::Array { exprs, .. } => exprs.iter().any(contain_agg_func),
        Expr::Map { kvs, .. } => kvs.iter().any(|(_, v)| contain_agg_func(v)),
        Expr::Interval { expr, .. } => contain_agg_func(expr),
        Expr::DateAdd { interval, date, .. } => {
            contain_agg_func(interval) || contain_agg_func(date)
        }
        Expr::DateSub { interval, date, .. } => {
            contain_agg_func(interval) || contain_agg_func(date)
        }
        Expr::DateTrunc { date, .. } => contain_agg_func(date),
    }
}
