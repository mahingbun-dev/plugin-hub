// `google.protobuf.FileDescriptorSet` 的最小解析器。
//
// 为什么手写 protobuf 线格式，而不是用 protobufjs 加载 `descriptor.proto` 再 decode：
//
//   1. **插件提交的 descriptor 只含自己的 proto**（不含 import 的依赖）。protobufjs 的
//      `decode` 不需要自包含，但想把它 load 成一个可用的 Root 就需要。Go 侧的
//      `conformance` 也刻意「只遍历不解析引用」——两边必须一致，否则会出现
//      「本地检查不过但中台接受」这种更糟的分歧。
//   2. 我们只要**消息的全限定名**这一件事，而 descriptor.proto 有上千行。
//      为了读三个字段去加载一个 DSL，代价与收益不成比例。
//
// 事实来源是 protobuf 的线格式规范；字段号取自
// `google/protobuf/descriptor.proto`（本仓库不带它，但字段号是**线上兼容性**的一部分，
// 一旦发布就不能改）。

/** 线格式里的类型。只列我们认得的四种。 */
const WIRE_VARINT = 0
const WIRE_FIXED64 = 1
const WIRE_LENGTH_DELIMITED = 2
const WIRE_FIXED32 = 5

/** 字段号。名字里的数字就是 descriptor.proto 里的编号。 */
const SET_FILE = 1
const FILE_PACKAGE = 2
const FILE_MESSAGE_TYPE = 4
const MESSAGE_NAME = 1
const MESSAGE_NESTED_TYPE = 3
const MESSAGE_OPTIONS = 7
const OPTIONS_MAP_ENTRY = 7

/** 解析失败。中台会把这类描述原样放进 `REJECT_CODE_DESCRIPTOR_INVALID` 的 message 里。 */
export class DescriptorError extends Error {
  constructor(message: string) {
    super(message)
    this.name = 'DescriptorError'
  }
}

/** 一个读游标。 */
class Reader {
  pos = 0
  private readonly buf: Uint8Array

  // 不用「构造器参数属性」（`constructor(private readonly buf: T)`）：那是**不可擦除**
  // 的 TS 语法，而本 SDK 靠 Node 的类型剥离直接跑 .ts（见 README 的选型），
  // 带参数属性的文件会在 `node xxx.ts` 时报 ERR_UNSUPPORTED_TYPESCRIPT_SYNTAX。
  constructor(buf: Uint8Array) {
    this.buf = buf
  }

  get done(): boolean {
    return this.pos >= this.buf.length
  }

  varint(): number {
    let result = 0
    let shift = 0
    for (;;) {
      if (this.pos >= this.buf.length) throw new DescriptorError('descriptor 被截断（varint）')
      const byte = this.buf[this.pos++]!
      // JS 的位运算在 32 位上做，超过 5 字节的 varint 只有长度/编号这类值才可能用到，
      // 而它们都在 2^32 以内。用乘加而不是 `|` 是为了保住 53 位的整数精度。
      result += (byte & 0x7f) * 2 ** shift
      if ((byte & 0x80) === 0) return result
      shift += 7
      if (shift > 63) throw new DescriptorError('varint 超过 63 位')
    }
  }

  bytes(): Uint8Array {
    const len = this.varint()
    if (this.pos + len > this.buf.length) throw new DescriptorError('descriptor 被截断（length）')
    const slice = this.buf.subarray(this.pos, this.pos + len)
    this.pos += len
    return slice
  }

  /** 跳过一段不关心的字段。 */
  skip(wireType: number): void {
    switch (wireType) {
      case WIRE_VARINT:
        this.varint()
        return
      case WIRE_FIXED64:
        this.pos += 8
        return
      case WIRE_LENGTH_DELIMITED:
        this.bytes()
        return
      case WIRE_FIXED32:
        this.pos += 4
        return
      default:
        throw new DescriptorError(`不认识的线类型 ${wireType}`)
    }
  }

  atEnd(): boolean {
    if (this.pos > this.buf.length) throw new DescriptorError('descriptor 被截断（越界）')
    return this.pos === this.buf.length
  }
}

/** 读一个字段的 tag，返回 `[字段号, 线类型]`。 */
function readTag(r: Reader): [number, number] {
  const tag = r.varint()
  return [Math.floor(tag / 8), tag % 8]
}

function readString(r: Reader): string {
  return Buffer.from(r.bytes()).toString('utf8')
}

/**
 * 解析 `FileDescriptorSet`，返回其中的消息全限定名。
 *
 * 只收 `message`，不收 enum / service——中台做字段级兼容检查的对象是消息。
 * 嵌套消息会带上外层的前缀（`Outer.Inner`），与中台侧的判据一致。
 */
export function descriptorMessages(raw: Uint8Array): Set<string> {
  const found = new Set<string>()
  const set = new Reader(raw)

  while (!set.atEnd()) {
    const [field, wire] = readTag(set)
    if (field === SET_FILE && wire === WIRE_LENGTH_DELIMITED) {
      collectFile(set.bytes(), found)
    } else {
      set.skip(wire)
    }
  }
  return found
}

function collectFile(raw: Uint8Array, into: Set<string>): void {
  const file = new Reader(raw)
  let pkg = ''

  // package 在文件里排在 message_type 前面（descriptor.proto 的字段号如此），
  // 但**不能依赖这个顺序**——编码方没有这个义务。所以先扫 package、再扫消息，
  // 两趟各自独立。
  const messageBlobs: Uint8Array[] = []

  while (!file.atEnd()) {
    const [field, wire] = readTag(file)
    if (field === FILE_PACKAGE && wire === WIRE_LENGTH_DELIMITED) {
      pkg = readString(file)
    } else if (field === FILE_MESSAGE_TYPE && wire === WIRE_LENGTH_DELIMITED) {
      messageBlobs.push(file.bytes())
    } else {
      file.skip(wire)
    }
  }

  for (const blob of messageBlobs) {
    collectMessage(blob, pkg, into)
  }
}

function collectMessage(raw: Uint8Array, prefix: string, into: Set<string>): void {
  const message = new Reader(raw)
  let name = ''
  let mapEntry = false
  const nested: Uint8Array[] = []

  while (!message.atEnd()) {
    const [field, wire] = readTag(message)
    if (field === MESSAGE_NAME && wire === WIRE_LENGTH_DELIMITED) {
      name = readString(message)
    } else if (field === MESSAGE_NESTED_TYPE && wire === WIRE_LENGTH_DELIMITED) {
      nested.push(message.bytes())
    } else if (field === MESSAGE_OPTIONS && wire === WIRE_LENGTH_DELIMITED) {
      mapEntry = readMapEntry(message.bytes())
    } else {
      message.skip(wire)
    }
  }

  // map 字段会生成合成的 XxxEntry 消息，属实现细节，不算契约类型
  if (mapEntry) return

  const fq = prefix ? `${prefix}.${name}` : name
  if (name) into.add(fq)
  for (const blob of nested) {
    collectMessage(blob, fq, into)
  }
}

function readMapEntry(raw: Uint8Array): boolean {
  const options = new Reader(raw)
  let mapEntry = false
  while (!options.atEnd()) {
    const [field, wire] = readTag(options)
    if (field === OPTIONS_MAP_ENTRY && wire === WIRE_VARINT) {
      mapEntry = options.varint() !== 0
    } else {
      options.skip(wire)
    }
  }
  return mapEntry
}

/**
 * 只用 `google.protobuf.Struct` 承载 JSON 的插件没有自己的 proto，返回空即可
 * ——中台允许空 descriptor，只要 manifest 里没声明自有类型。
 *
 * 需要与其他插件在 flow 里传递**类型化**消息时，得提供一份自己 proto 的
 * FileDescriptorSet。本 SDK 目前不代劳这件事（Go 侧靠 protoc 生成的
 * `protoreflect.FileDescriptor`，Node 侧没有等价物）；模板里的 `descriptor()`
 * 因此返回空，并注明升版本的注意事项。
 */
export function emptyDescriptor(): Uint8Array {
  return new Uint8Array(0)
}
