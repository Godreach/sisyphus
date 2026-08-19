// when 受限表达式：accept/reject 端口（票 B4-T7，ADR-0006）。
//
// `sisyphus-model/src/when.rs` 的忠实端口——只判合法性（`isValidWhen`），不产 AST、
// 不求值（求值在 model 内、Server 独享，ADR-0009）。语言受限、无图灵完备：比较、
// `&&`/`||`、字符串相等、存在性判断。越界语法 reject。
//
// parity 陷阱（与 Rust 逐字节对齐）：
// - 数字消费循环只吃 `[0-9.]`——前导 `-` 吃不掉 → 空文本 → reject（Rust 数字 arm 的
//   `-` 守卫实为死代码，负数字面量不支持）。用 `Number.isFinite(Number(text))` 严格
//   判定，禁 `parseFloat`（会错放 `parseFloat("-5") === -5`）。
// - `true`/`false` tokenize 为字符串字面量（非布尔）。
// - `exists` 须跟 Ident 操作数（`${X}` 或裸字）；数字/字符串/括号 → reject。
// - `${name}`：name 须非空且仅 `[A-Za-z0-9_]`（数字亦可，与 variables::is_valid_name 同）。
// - 尾随裸 `$`（无 `{`）reject 不崩（Rust 曾因 let-chain 求值序越界 panic，已修）。

/** 合法返回 true，否则 false（与 `when::parse` 的 accept/reject 对账）。 */
export function isValidWhen(source: string): boolean {
  let tokens: Tok[]
  try {
    tokens = tokenize(source)
  } catch {
    return false
  }
  try {
    const parser = new Parser(tokens)
    parser.parseOr()
    // 全部 token 须消费尽——尾随垃圾 reject（Rust `parser.pos < tokens.len()`）。
    return parser.pos === tokens.length
  } catch {
    return false
  }
}

// ---------------------------------------------------------------------------
// 词法（与 when.rs tokenize 同构）
// ---------------------------------------------------------------------------

type Tok =
  | { t: 'ident'; v: string }
  | { t: 'str'; v: string }
  | { t: 'num' }
  | { t: 'and' }
  | { t: 'or' }
  | { t: 'eq' }
  | { t: 'ne' }
  | { t: 'lt' }
  | { t: 'le' }
  | { t: 'gt' }
  | { t: 'ge' }
  | { t: 'lparen' }
  | { t: 'rparen' }
  | { t: 'exists' }

class WhenReject {}

function isAsciiDigit(c: string): boolean {
  return c >= '0' && c <= '9'
}
function isAsciiAlpha(c: string): boolean {
  return (c >= 'a' && c <= 'z') || (c >= 'A' && c <= 'Z')
}
function isIdentChar(c: string): boolean {
  return isAsciiAlpha(c) || isAsciiDigit(c) || c === '_'
}

function tokenize(src: string): Tok[] {
  const toks: Tok[] = []
  let i = 0
  const n = src.length
  while (i < n) {
    const c = src[i]!
    switch (c) {
      case ' ':
      case '\t':
      case '\n':
      case '\r':
        i++
        break
      case '(':
        toks.push({ t: 'lparen' })
        i++
        break
      case ')':
        toks.push({ t: 'rparen' })
        i++
        break
      case '&':
        if (src[i + 1] === '&') {
          toks.push({ t: 'and' })
          i += 2
        } else {
          throw new WhenReject()
        }
        break
      case '|':
        if (src[i + 1] === '|') {
          toks.push({ t: 'or' })
          i += 2
        } else {
          throw new WhenReject()
        }
        break
      case '=':
        if (src[i + 1] === '=') {
          toks.push({ t: 'eq' })
          i += 2
        } else {
          throw new WhenReject()
        }
        break
      case '!':
        if (src[i + 1] === '=') {
          toks.push({ t: 'ne' })
          i += 2
        } else {
          throw new WhenReject()
        }
        break
      case '<':
        if (src[i + 1] === '=') {
          toks.push({ t: 'le' })
          i += 2
        } else {
          toks.push({ t: 'lt' })
          i++
        }
        break
      case '>':
        if (src[i + 1] === '=') {
          toks.push({ t: 'ge' })
          i += 2
        } else {
          toks.push({ t: 'gt' })
          i++
        }
        break
      case '"': {
        // 字符串字面量：读到下一个 `"`；无转义（`"` 即闭合），未闭合 reject。
        i++
        let s = ''
        let closed = false
        while (i < n) {
          const ch = src[i]!
          if (ch === '"') {
            closed = true
            i++
            break
          }
          s += ch
          i++
        }
        if (!closed) {
          throw new WhenReject()
        }
        toks.push({ t: 'str', v: s })
        break
      }
      case '$': {
        // `${name}` 变量引用。先守卫 `${`，再找 `}`——避免尾随裸 `$` 越界。
        if (src[i + 1] === '{') {
          const rest = src.slice(i + 2)
          const endRel = rest.indexOf('}')
          if (endRel >= 0) {
            const name = rest.slice(0, endRel)
            if (name.length > 0 && [...name].every(isIdentChar)) {
              toks.push({ t: 'ident', v: name })
              i = i + 2 + endRel + 1
              break
            }
          }
        }
        throw new SyntaxError()
      }
      default:
        if (isAsciiDigit(c) || c === '-' || c === '.') {
          // 数字：消费循环只吃 [0-9.]——前导 `-` 吃不掉 → 空文本 → reject。
          const start = i
          while (i < n && (isAsciiDigit(src[i]!) || src[i] === '.')) {
            i++
          }
          const text = src.slice(start, i)
          if (text === '' || !Number.isFinite(Number(text))) {
            throw new WhenReject()
          }
          toks.push({ t: 'num' })
          break
        }
        if (isAsciiAlpha(c) || c === '_') {
          const start = i
          while (i < n && isIdentChar(src[i]!)) {
            i++
          }
          const word = src.slice(start, i)
          if (word === 'exists') {
            toks.push({ t: 'exists' })
          } else if (word === 'true' || word === 'false') {
            toks.push({ t: 'str', v: word })
          } else {
            toks.push({ t: 'ident', v: word })
          }
          break
        }
        throw new SyntaxError()
    }
  }
  return toks
}

// ---------------------------------------------------------------------------
// 语法（递归下降，优先级：or < and < cmp < primary；与 when.rs 同构）
// ---------------------------------------------------------------------------

class Parser {
  private readonly toks: Tok[]
  /** 当前位置（与 Rust `parser.pos` 同）。 */
  pos = 0

  constructor(toks: Tok[]) {
    this.toks = toks
  }

  private peek(): Tok | undefined {
    return this.toks[this.pos]
  }

  private next(): Tok | undefined {
    return this.toks[this.pos++]
  }

  parseOr(): void {
    this.parseAnd()
    while (this.peek()?.t === 'or') {
      this.next()
      this.parseAnd()
    }
  }

  parseAnd(): void {
    this.parseCmp()
    while (this.peek()?.t === 'and') {
      this.next()
      this.parseCmp()
    }
  }

  parseCmp(): void {
    this.parsePrimary()
    const t = this.peek()?.t
    if (t === 'eq' || t === 'ne' || t === 'lt' || t === 'le' || t === 'gt' || t === 'ge') {
      this.next()
      this.parsePrimary()
    }
  }

  parsePrimary(): void {
    const tok = this.next()
    if (tok === undefined) {
      // UnexpectedEnd。
      throw new SyntaxError()
    }
    switch (tok.t) {
      case 'lparen':
        this.parseOr()
        if (this.next()?.t !== 'rparen') {
          // UnbalancedParens。
          throw new WhenReject()
        }
        break
      case 'str':
      case 'num':
      case 'ident':
        // Literal / Var。
        break
      case 'exists': {
        // exists 须跟 Ident 操作数。
        const op = this.next()
        if (op === undefined || op.t !== 'ident') {
          // MissingOperand。
          throw new WhenReject()
        }
        break
      }
      default:
        // 操作符 token 不能作 primary。
        throw new SyntaxError()
    }
  }
}
