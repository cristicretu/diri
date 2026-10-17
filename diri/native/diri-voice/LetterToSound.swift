// Letter-to-sound rules for words Kokoro's lexicons do not know (names,
// jargon). Written for diri in the style of the public-domain NRL rules
// (Elovitz, Johnson, McHugh and Shore, 1976): each rule rewrites a run of
// letters given its left and right context, producing ARPAbet, which is then
// mapped to misaki's phoneme symbols with one primary stress.
//
// Context symbols: " " word edge, "#" one or more vowels, ":" zero or more
// consonants, "^" one consonant, "." one voiced consonant, "+" e/i/y,
// "%" a suffix (er e es ed ing ely), "&" a sibilant, "@" a consonant that
// makes a following u long (t s r d l z n j th ch sh).

import Foundation

final class LetterToSound {
    private struct Rule {
        let left: [Character]
        let match: [Character]
        let right: [Character]
        let phones: [String]
    }

    private let rules: [Character: [Rule]]

    init() {
        var rules: [Character: [Rule]] = [:]
        for (left, match, right, phones) in letterRules {
            let rule = Rule(
                left: Array(left), match: Array(match), right: Array(right),
                phones: phones.split(separator: " ").map(String.init))
            rules[rule.match[0], default: []].append(rule)
        }
        self.rules = rules
    }

    /// Kokoro phonemes for a lowercase word, or nil when it has no letters.
    func phonemes(_ word: String, british: Bool) -> String? {
        let letters = word.lowercased().filter { $0 >= "a" && $0 <= "z" }
        guard !letters.isEmpty else { return nil }
        let text = Array(" " + letters + " ")
        var phones: [String] = []
        var index = 1
        while index < text.count - 1 {
            let c = text[index]
            var matched = false
            for rule in rules[c] ?? [] where matches(rule, text, index) {
                phones.append(contentsOf: rule.phones)
                index += rule.match.count
                matched = true
                break
            }
            if !matched { index += 1 }
        }
        return render(phones, word: letters, british: british)
    }

    // MARK: Matching

    private func matches(_ rule: Rule, _ text: [Character], _ index: Int) -> Bool {
        let end = index + rule.match.count
        guard end <= text.count, Array(text[index..<end]) == rule.match else { return false }
        return matchesLeft(rule.left, text, index - 1) && matchesRight(rule.right, text, end)
    }

    private func matchesLeft(_ pattern: [Character], _ text: [Character], _ start: Int) -> Bool {
        var position = start
        for symbol in pattern.reversed() {
            switch symbol {
            case " ":
                guard position < 0 || !isLetter(text[position]) else { return false }
                position -= 1
            case "#":
                guard position >= 0, isVowel(text[position]) else { return false }
                while position >= 0, isVowel(text[position]) { position -= 1 }
            case ":":
                while position >= 0, isConsonant(text[position]) { position -= 1 }
            case "^":
                guard position >= 0, isConsonant(text[position]) else { return false }
                position -= 1
            case ".":
                guard position >= 0, "bdvgjlmnrwz".contains(text[position]) else { return false }
                position -= 1
            case "+":
                guard position >= 0, "eiy".contains(text[position]) else { return false }
                position -= 1
            case "&":
                if position >= 1, ["ch", "sh"].contains(String(text[(position - 1)...position])) {
                    position -= 2
                } else if position >= 0, "scgzxj".contains(text[position]) {
                    position -= 1
                } else {
                    return false
                }
            case "@":
                if position >= 1, ["th", "ch", "sh"].contains(String(text[(position - 1)...position])) {
                    position -= 2
                } else if position >= 0, "tsrdlznj".contains(text[position]) {
                    position -= 1
                } else {
                    return false
                }
            default:
                guard position >= 0, text[position] == symbol else { return false }
                position -= 1
            }
        }
        return true
    }

    private func matchesRight(_ pattern: [Character], _ text: [Character], _ start: Int) -> Bool {
        var position = start
        for symbol in pattern {
            switch symbol {
            case " ":
                guard position >= text.count || !isLetter(text[position]) else { return false }
                position += 1
            case "#":
                guard position < text.count, isVowel(text[position]) else { return false }
                while position < text.count, isVowel(text[position]) { position += 1 }
            case ":":
                while position < text.count, isConsonant(text[position]) { position += 1 }
            case "^":
                guard position < text.count, isConsonant(text[position]) else { return false }
                position += 1
            case ".":
                guard position < text.count, "bdvgjlmnrwz".contains(text[position]) else { return false }
                position += 1
            case "+":
                guard position < text.count, "eiy".contains(text[position]) else { return false }
                position += 1
            case "%":
                let rest = String(text[min(position, text.count)...])
                guard let suffix = ["ing", "ely", "er", "es", "ed", "e"].first(where: { rest.hasPrefix($0) })
                else { return false }
                position += suffix.count
            default:
                guard position < text.count, text[position] == symbol else { return false }
                position += 1
            }
        }
        return true
    }

    private func isLetter(_ c: Character) -> Bool { c >= "a" && c <= "z" }
    private func isVowel(_ c: Character) -> Bool { "aeiouy".contains(c) }
    private func isConsonant(_ c: Character) -> Bool { isLetter(c) && !isVowel(c) }

    // MARK: ARPAbet to Kokoro

    private static let vowelPhones: Set<String> = [
        "AA", "AE", "AH", "AO", "AW", "AX", "AY", "EH", "ER", "EY", "IH", "IY", "OW", "OY", "UH", "UW",
    ]

    private func render(_ phones: [String], word: String, british: Bool) -> String? {
        let vowelIndices = phones.indices.filter { Self.vowelPhones.contains(phones[$0]) }
        guard !phones.isEmpty else { return nil }
        // English stresses the first syllable, except after an unstressed
        // prefix (a-, be-, de-, re-, ex-, con-).
        var stressed = vowelIndices.first
        if vowelIndices.count > 1, let first = vowelIndices.first,
            phones[first] == "AX"
                || (["IH", "EH"].contains(phones[first])
                    && ["be", "de", "re", "ex", "pre", "con", "com"].contains { word.hasPrefix($0) && word.count > 5 })
        {
            stressed = vowelIndices[1]
        }
        var out = ""
        for (index, phone) in phones.enumerated() {
            let isStressed = index == stressed
            let nextIsVowel = index + 1 < phones.count && Self.vowelPhones.contains(phones[index + 1])
            if isStressed { out += "ˈ" }
            // An R between consonants or at the end after one is syllabic:
            // "Wispr" reads wisper.
            if phone == "R", index > 0, !Self.vowelPhones.contains(phones[index - 1]), !nextIsVowel {
                out += british ? "ə" : "əɹ"
                continue
            }
            out += symbol(phone, stressed: isStressed, british: british, beforeVowel: nextIsVowel)
        }
        return out.isEmpty ? nil : out
    }

    private func symbol(_ phone: String, stressed: Bool, british: Bool, beforeVowel: Bool) -> String {
        switch phone {
        case "AA": return british ? "ɑː" : "ɑ"
        case "AE": return british ? "a" : "æ"
        case "AH": return stressed ? "ʌ" : "ə"
        case "AO": return british ? "ɔː" : "ɔ"
        case "AW": return "W"
        case "AX": return "ə"
        case "AY": return "I"
        case "EH": return "ɛ"
        case "ER": return british ? (stressed ? "ɜː" : "ə") : (stressed ? "ɜɹ" : "əɹ")
        case "EY": return "A"
        case "IH": return "ɪ"
        case "IY": return british && stressed ? "iː" : "i"
        case "OW": return british ? "Q" : "O"
        case "OY": return "Y"
        case "UH": return "ʊ"
        case "UW": return british ? "uː" : "u"
        case "B": return "b"
        case "CH": return "ʧ"
        case "D": return "d"
        case "DH": return "ð"
        case "F": return "f"
        case "G": return "ɡ"
        case "HH": return "h"
        case "JH": return "ʤ"
        case "K": return "k"
        case "L": return "l"
        case "M": return "m"
        case "N": return "n"
        case "NG": return "ŋ"
        case "P": return "p"
        case "R": return british && !beforeVowel ? "" : "ɹ"
        case "S": return "s"
        case "SH": return "ʃ"
        case "T": return "t"
        case "TH": return "θ"
        case "V": return "v"
        case "W": return "w"
        case "Y": return "j"
        case "Z": return "z"
        case "ZH": return "ʒ"
        default: return ""
        }
    }
}

/// (left context, letters, right context, ARPAbet). Within a letter, the
/// first matching rule wins, so specific rules come first.
private let letterRules: [(String, String, String, String)] = [
    // a
    ("", "a", " ", "AX"),
    (" ", "are", " ", "AA R"),
    (" ", "ar", "o", "AX R"),
    ("", "ar", "#", "EH R"),
    ("^", "as", "#", "EY S"),
    ("", "a", "wa", "AX"),
    ("", "aw", "", "AO"),
    (" :", "any", "", "EH N IY"),
    ("", "a", "^+#", "EY"),
    ("#:", "ally", "", "AX L IY"),
    (" ", "al", "#", "AX L"),
    ("", "again", "", "AX G EH N"),
    ("#:", "ag", "e", "IH JH"),
    ("", "a", "^+:#", "AE"),
    (" :", "a", "^+ ", "EY"),
    ("", "a", "^%", "EY"),
    (" ", "arr", "", "AX R"),
    ("", "arr", "", "AE R"),
    (" :", "ar", " ", "AA R"),
    ("", "ar", " ", "ER"),
    ("", "ar", "", "AA R"),
    ("", "air", "", "EH R"),
    ("", "ai", "", "EY"),
    ("", "ay", "", "EY"),
    ("", "au", "", "AO"),
    ("#:", "al", " ", "AX L"),
    ("#:", "als", " ", "AX L Z"),
    ("", "alk", "", "AO K"),
    ("", "al", "^", "AO L"),
    (" :", "able", "", "EY B AX L"),
    ("", "able", "", "AX B AX L"),
    ("", "ang", "+", "EY N JH"),
    ("", "a", "", "AE"),
    // b
    (" ", "be", "^#", "B IH"),
    ("", "being", "", "B IY IH NG"),
    (" ", "both", " ", "B OW TH"),
    (" ", "bus", "#", "B IH Z"),
    ("", "buil", "", "B IH L"),
    ("", "bb", "", "B"),
    ("", "b", "", "B"),
    // c
    (" ", "ch", "^", "K"),
    ("^e", "ch", "", "K"),
    ("", "ch", "", "CH"),
    (" s", "ci", "#", "S AY"),
    ("", "ci", "a", "SH"),
    ("", "ci", "o", "SH"),
    ("", "ci", "en", "SH"),
    ("", "c", "+", "S"),
    ("", "ck", "", "K"),
    ("", "com", "%", "K AH M"),
    ("", "cc", "+", "K S"),
    ("", "cc", "", "K"),
    ("", "c", "", "K"),
    // d
    ("#:", "ded", " ", "D IH D"),
    (".e", "d", " ", "D"),
    ("#:^e", "d", " ", "T"),
    (" ", "de", "^#", "D IH"),
    (" ", "do", " ", "D UW"),
    (" ", "does", "", "D AH Z"),
    (" ", "doing", "", "D UW IH NG"),
    (" ", "dow", "", "D AW"),
    ("", "du", "a", "JH UW"),
    ("", "dd", "", "D"),
    ("", "d", "", "D"),
    // e
    ("#:", "e", " ", ""),
    (" :", "e", " ", "IY"),
    ("#", "ed", " ", "D"),
    ("#:", "e", "d ", ""),
    ("", "ev", "er", "EH V"),
    ("", "e", "^%", "IY"),
    ("", "eri", "#", "IY R IY"),
    ("", "eri", "", "EH R IH"),
    ("#:", "er", "#", "ER"),
    ("", "er", "#", "EH R"),
    ("", "er", "", "ER"),
    (" ", "even", "", "IY V EH N"),
    ("#:", "e", "w", ""),
    ("@", "ew", "", "UW"),
    ("", "ew", "", "Y UW"),
    ("", "e", "o", "IY"),
    ("#:&", "es", " ", "IH Z"),
    ("#:", "e", "s ", ""),
    ("#:", "ely", " ", "L IY"),
    ("#:", "ement", "", "M EH N T"),
    ("", "eful", "", "F UH L"),
    ("", "ee", "", "IY"),
    ("", "earn", "", "ER N"),
    (" ", "ear", "^", "ER"),
    ("", "ead", "", "EH D"),
    ("#:", "ea", " ", "IY AX"),
    ("", "ea", "su", "EH"),
    ("", "ea", "", "IY"),
    ("", "eigh", "", "EY"),
    ("", "ei", "", "IY"),
    (" ", "eye", "", "AY"),
    ("", "ey", "", "IY"),
    ("", "eu", "", "Y UW"),
    ("", "e", "", "EH"),
    // f
    ("", "ful", "", "F UH L"),
    ("", "ff", "", "F"),
    ("", "f", "", "F"),
    // g
    ("", "giv", "", "G IH V"),
    (" ", "g", "i^", "G"),
    ("", "ge", "t", "G EH"),
    ("su", "gges", "", "G JH EH S"),
    ("", "gg", "", "G"),
    (" b#", "g", "", "G"),
    ("", "g", "+", "JH"),
    ("", "great", "", "G R EY T"),
    ("#", "gh", "", ""),
    ("", "g", "", "G"),
    // h
    (" ", "hav", "", "HH AE V"),
    (" ", "here", "", "HH IY R"),
    (" ", "hour", "", "AW ER"),
    ("", "how", "", "HH AW"),
    ("", "h", "#", "HH"),
    ("", "h", "", ""),
    // i
    (" ", "in", "", "IH N"),
    (" ", "i", " ", "AY"),
    ("", "in", "d", "AY N"),
    ("", "ier", "", "IY ER"),
    ("#:r", "ied", "", "IY D"),
    ("", "ied", " ", "AY D"),
    ("", "ien", "", "IY EH N"),
    ("", "ie", "t", "AY EH"),
    (" :", "i", "%", "AY"),
    ("", "i", "%", "IY"),
    ("", "ie", "", "IY"),
    ("", "i", " ", "IY"),
    ("", "i", "^+:#", "IH"),
    ("", "ir", "#", "AY R"),
    ("", "iz", "%", "AY Z"),
    ("", "is", "%", "AY Z"),
    ("", "i", "d%", "AY"),
    ("+^", "i", "^+", "IH"),
    ("", "i", "t%", "AY"),
    ("#:^", "i", "^+", "IH"),
    ("", "i", "^+", "AY"),
    ("", "ir", "", "ER"),
    ("", "igh", "", "AY"),
    ("", "ild", "", "AY L D"),
    ("", "ign", " ", "AY N"),
    ("", "ign", "^", "AY N"),
    ("", "ign", "%", "AY N"),
    ("", "ique", "", "IY K"),
    ("^", "i", "#", "IY"),
    ("", "i", "", "IH"),
    // j
    ("", "j", "", "JH"),
    // k
    (" ", "k", "n", ""),
    ("", "k", "", "K"),
    // l
    ("", "lo", "c#", "L OW"),
    ("l", "l", "", ""),
    ("#:^", "l", "%", "AX L"),
    ("", "lead", "", "L IY D"),
    ("", "l", "", "L"),
    // m
    ("", "mov", "", "M UW V"),
    ("", "mm", "", "M"),
    ("", "m", "", "M"),
    // n
    ("e", "ng", "+", "N JH"),
    ("", "ng", "r", "NG G"),
    ("", "ng", "#", "NG G"),
    ("", "ngl", "%", "NG G AX L"),
    ("", "ng", "", "NG"),
    ("", "nk", "", "NG K"),
    (" ", "now", " ", "N AW"),
    ("", "nn", "", "N"),
    ("", "n", "", "N"),
    // o
    ("", "of", " ", "AX V"),
    ("", "orough", "", "ER OW"),
    ("#:", "or", " ", "ER"),
    ("#:", "ors", " ", "ER Z"),
    ("", "or", "", "AO R"),
    (" ", "one", "", "W AH N"),
    ("", "ow", "", "OW"),
    (" ", "over", "", "OW V ER"),
    ("", "ov", "", "AH V"),
    ("", "o", "^%", "OW"),
    ("", "o", "^en", "OW"),
    ("", "o", "^i#", "OW"),
    ("", "ol", "d", "OW L"),
    ("", "ought", "", "AO T"),
    ("", "ough", "", "AH F"),
    (" ", "ou", "", "AW"),
    ("h", "ou", "s#", "AW"),
    ("", "ous", "", "AX S"),
    ("", "our", "", "AO R"),
    ("", "ould", "", "UH D"),
    ("^", "ou", "^l", "AH"),
    ("", "oup", "", "UW P"),
    ("", "ou", "", "AW"),
    ("", "oy", "", "OY"),
    ("", "oing", "", "OW IH NG"),
    ("", "oi", "", "OY"),
    ("", "oor", "", "AO R"),
    ("", "ook", "", "UH K"),
    ("", "ood", "", "UH D"),
    ("", "oo", "", "UW"),
    ("", "o", "e", "OW"),
    ("", "o", " ", "OW"),
    ("", "oa", "", "OW"),
    (" ", "only", "", "OW N L IY"),
    (" ", "once", "", "W AH N S"),
    ("c", "o", "n", "AA"),
    ("", "o", "ng", "AO"),
    (" :^", "o", "n", "AH"),
    ("i", "on", "", "AX N"),
    ("#:", "on", " ", "AX N"),
    ("#^", "on", "", "AX N"),
    ("", "o", "st ", "OW"),
    ("", "of", "^", "AO F"),
    ("", "other", "", "AH DH ER"),
    ("", "oss", " ", "AO S"),
    ("#:^", "om", "", "AH M"),
    ("^", "o", "^#", "OW"),
    ("", "o", "", "AA"),
    // p
    ("", "ph", "", "F"),
    ("", "peop", "", "P IY P"),
    ("", "pow", "", "P AW"),
    ("", "put", " ", "P UH T"),
    ("", "pp", "", "P"),
    ("", "p", "", "P"),
    // q
    ("", "quar", "", "K W AO R"),
    ("", "qu", "", "K W"),
    ("", "q", "", "K"),
    // r
    (" ", "re", "^#", "R IY"),
    ("", "rr", "", "R"),
    ("", "r", "", "R"),
    // s
    ("", "sh", "", "SH"),
    ("#", "sion", "", "ZH AX N"),
    ("", "some", "", "S AH M"),
    ("#", "sur", "#", "ZH ER"),
    ("", "sur", "#", "SH ER"),
    ("#", "su", "#", "ZH UW"),
    ("#", "ssu", "#", "SH UW"),
    ("#", "sed", " ", "Z D"),
    ("#", "s", "#", "Z"),
    ("", "said", "", "S EH D"),
    ("^", "sion", "", "SH AX N"),
    ("", "s", "s", ""),
    (".", "s", " ", "Z"),
    ("#:.e", "s", " ", "Z"),
    ("#:^#", "s", " ", "S"),
    ("u", "s", " ", "S"),
    (" :#", "s", " ", "Z"),
    (" ", "sch", "", "S K"),
    ("", "s", "c+", ""),
    ("#", "sm", "", "Z M"),
    ("", "s", "", "S"),
    // t
    (" ", "the", " ", "DH AX"),
    ("", "to", " ", "T UW"),
    ("", "that", " ", "DH AE T"),
    (" ", "this", " ", "DH IH S"),
    (" ", "they", "", "DH EY"),
    (" ", "there", "", "DH EH R"),
    ("", "ther", "", "DH ER"),
    ("", "their", "", "DH EH R"),
    (" ", "than", " ", "DH AE N"),
    (" ", "them", " ", "DH EH M"),
    ("", "these", " ", "DH IY Z"),
    (" ", "then", "", "DH EH N"),
    ("", "through", "", "TH R UW"),
    ("", "those", "", "DH OW Z"),
    ("", "though", " ", "DH OW"),
    (" ", "thus", "", "DH AH S"),
    ("", "th", "", "TH"),
    ("#:", "ted", " ", "T IH D"),
    ("s", "ti", "#n", "CH"),
    ("", "ti", "o", "SH"),
    ("", "ti", "a", "SH"),
    ("", "tien", "", "SH AX N"),
    ("", "tur", "#", "CH ER"),
    ("", "tu", "a", "CH UW"),
    (" ", "two", "", "T UW"),
    ("", "tch", "", "CH"),
    ("", "tt", "", "T"),
    ("", "t", "", "T"),
    // u
    (" ", "un", "i", "Y UW N"),
    (" ", "un", "", "AH N"),
    (" ", "upon", "", "AX P AO N"),
    ("@", "ur", "#", "UH R"),
    ("", "ur", "#", "Y UH R"),
    ("", "ur", "", "ER"),
    ("", "u", "^ ", "AH"),
    ("", "u", "^^", "AH"),
    ("", "uy", "", "AY"),
    (" g", "u", "#", ""),
    ("g", "u", "%", ""),
    ("g", "u", "#", "W"),
    ("#n", "u", "", "Y UW"),
    ("@", "u", "", "UW"),
    ("", "u", "", "Y UW"),
    // v
    ("", "view", "", "V Y UW"),
    ("", "v", "", "V"),
    // w
    (" ", "were", "", "W ER"),
    ("", "wa", "s", "W AA"),
    ("", "wa", "t", "W AA"),
    ("", "where", "", "W EH R"),
    ("", "what", "", "W AA T"),
    ("", "whol", "", "HH OW L"),
    ("", "who", "", "HH UW"),
    ("", "wh", "", "W"),
    ("", "war", "", "W AO R"),
    ("", "wor", "^", "W ER"),
    ("", "wr", "", "R"),
    ("", "w", "", "W"),
    // x
    (" ", "x", "", "Z"),
    ("", "x", "", "K S"),
    // y
    ("", "young", "", "Y AH NG"),
    (" ", "you", "", "Y UW"),
    (" ", "yes", "", "Y EH S"),
    (" ", "y", "", "Y"),
    ("#:^", "y", " ", "IY"),
    ("#:^", "y", "i", "IY"),
    (" :", "y", " ", "AY"),
    (" :", "y", "#", "AY"),
    (" :", "y", "^+:#", "IH"),
    (" :", "y", "^#", "AY"),
    ("", "y", "", "IH"),
    // z
    ("", "zz", "", "Z"),
    ("", "z", "", "Z"),
]
