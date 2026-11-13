import Foundation

public enum SafeText {
  public static let redactionMarker = "[redacted]"

  public static func sanitize(_ text: String, limit: Int) -> String {
    var scalars = String.UnicodeScalarView()
    for scalar in text.unicodeScalars {
      if scalar == "\n" || scalar == "\t" {
        scalars.append(scalar)
      } else if scalar.properties.generalCategory == .control
        || scalar.properties.generalCategory == .format
      {
        continue
      } else {
        scalars.append(scalar)
      }
    }
    let redacted = redactLongTokens(String(scalars))
    guard redacted.count > limit else { return redacted }
    return String(redacted.prefix(max(0, limit))) + "…"
  }

  public static func tail(_ data: Data, limit: Int) -> String {
    let text = String(decoding: data.suffix(limit * 4), as: UTF8.self)
    let trimmed = text.trimmingCharacters(in: .whitespacesAndNewlines)
    guard trimmed.count > limit else { return sanitize(trimmed, limit: limit) }
    return "…" + sanitize(String(trimmed.suffix(limit)), limit: limit)
  }

  static func redactLongTokens(_ text: String) -> String {
    var output = ""
    var token = ""
    func flush() {
      output += token.count >= 40 ? redactionMarker : token
      token = ""
    }
    for character in text {
      if character.isASCII, character.isLetter || character.isNumber || "+/=_-".contains(character)
      {
        token.append(character)
      } else {
        flush()
        output.append(character)
      }
    }
    flush()
    return output
  }
}

public enum EndpointIdentifier {
  public static func normalize(_ raw: String) -> String? {
    let value = raw.trimmingCharacters(in: .whitespacesAndNewlines).lowercased()
    guard value.count == 64, value.allSatisfy({ $0.isHexDigit && $0.isASCII }) else { return nil }
    return value
  }

  public static func abbreviated(_ value: String) -> String {
    guard value.count > 16 else { return value }
    return "\(value.prefix(8))…\(value.suffix(8))"
  }
}

public enum Formatting {
  public static func bytes(_ value: UInt64, locale: Locale = .current) -> String {
    let units = ["B", "KiB", "MiB", "GiB", "TiB", "PiB", "EiB"]
    var amount = Double(value)
    var index = 0
    while amount >= 1024 && index < units.count - 1 {
      amount /= 1024
      index += 1
    }
    return amount.formatted(.number.locale(locale).precision(.fractionLength(0...2))) + " "
      + units[index]
  }

  public static func captureDate(_ raw: String) -> String {
    let parser = ISO8601DateFormatter()
    parser.formatOptions = [.withInternetDateTime, .withFractionalSeconds]
    let date = parser.date(from: raw) ?? ISO8601DateFormatter().date(from: raw)
    return date?.formatted(date: .abbreviated, time: .standard) ?? SafeText.sanitize(raw, limit: 64)
  }

  public static func mib(_ value: UInt64, locale: Locale = .current) -> String {
    if value >= 1024 * 1024 {
      return (Double(value) / (1024 * 1024)).formatted(
        .number.locale(locale).precision(.fractionLength(1))) + " TiB"
    }
    if value >= 1024 {
      return (Double(value) / 1024).formatted(
        .number.locale(locale).precision(.fractionLength(0...1))) + " GiB"
    }
    return value.formatted(.number.locale(locale)) + " MiB"
  }

  public static func fraction(_ used: UInt64, of total: UInt64) -> Double {
    guard total > 0 else { return used > 0 ? 1 : 0 }
    return min(1, Double(used) / Double(total))
  }
}
