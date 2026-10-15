// English grapheme-to-phoneme conversion for Kokoro.
//
// A Swift port of misaki's English G2P (hexgrad/misaki, misaki/en.py,
// Apache-2.0, https://github.com/hexgrad/misaki), the phonemizer Kokoro was
// trained with. It reads misaki's own gold and silver lexicons, so words in
// them sound exactly as Kokoro expects. Differences from the original:
//
// - no spaCy: a small heuristic tagger stands in for part-of-speech tags,
//   which only pick between a word's pronunciations (read / read);
// - no num2words (LGPL): numbers, ordinals and years are spelled here;
// - no espeak-ng (GPL): words missing from the lexicons go through
//   `LetterToSound`, after trying letter-by-letter acronyms and compounds.
//
// Output uses only symbols in Kokoro's vocabulary.

import Foundation

/// misaki-compatible English G2P for Kokoro (Apache-2.0 port of hexgrad/misaki en.py).
final class EnglishG2P {
    enum LexiconError: Error { case unreadable }

    private let british: Bool
    private let golds: NSDictionary
    private let silvers: NSDictionary
    private let letterToSound = LetterToSound()

    /// `gold`/`silver`: the contents of misaki's us_gold.json/us_silver.json (or gb_*.json when british).
    init(gold: Data, silver: Data, british: Bool) throws {
        guard let golds = try JSONSerialization.jsonObject(with: gold) as? NSDictionary,
            let silvers = try JSONSerialization.jsonObject(with: silver) as? NSDictionary
        else { throw LexiconError.unreadable }
        self.golds = golds
        self.silvers = silvers
        self.british = british
    }

    /// Kokoro phoneme string for `text`, in misaki's output format (words separated by spaces, punctuation kept as misaki keeps it).
    func phonemes(_ text: String) -> String {
        let tokens = G2PTokenizer.tokenize(G2PTokenizer.preprocess(text))
        guard !tokens.isEmpty else { return "" }
        let words = retokenize(tokens)
        var ctx = TokenContext()
        for word in words.reversed() {
            switch word {
            case .single(let token):
                if token.phonemes == nil {
                    (token.phonemes, token.rating) = lexicon(token, ctx)
                }
                if token.phonemes == nil {
                    token.phonemes = fallback(token.text)
                }
                ctx = tokenContext(ctx, token.phonemes, token)
            case .group(let group):
                resolveGroup(group, &ctx)
            }
        }
        var result = ""
        for word in words {
            let token: MToken
            switch word {
            case .single(let single): token = single
            case .group(let group): token = mergeTokens(group, unknown: "")
            }
            let phonemes = (token.phonemes ?? "")
                .replacingOccurrences(of: "ɾ", with: "T")
                .replacingOccurrences(of: "ʔ", with: "t")
            result += phonemes + token.whitespace
        }
        let kept = result.unicodeScalars.filter { kokoroVocabulary.contains($0) || $0 == " " }
        var spoken = String(String.UnicodeScalarView(kept))
        while spoken.contains("  ") {
            spoken = spoken.replacingOccurrences(of: "  ", with: " ")
        }
        return spoken.trimmingCharacters(in: .whitespaces)
    }

    // MARK: Words made of several subtokens ("GitHub's", "well-known", "15th")

    private func resolveGroup(_ group: [MToken], _ ctx: inout TokenContext) {
        var left = 0
        var right = group.count
        var shouldFallback = false
        while left < right {
            let slice = Array(group[left..<right])
            let merged: MToken? =
                slice.contains { $0.alias != nil || $0.phonemes != nil } ? nil : mergeTokens(slice, unknown: nil)
            let (ps, rating) = merged.map { lexicon($0, ctx) } ?? (nil, nil)
            if let ps, let merged {
                group[left].phonemes = ps
                group[left].rating = rating
                for token in group[(left + 1)..<right] {
                    token.phonemes = ""
                    token.rating = rating
                }
                ctx = tokenContext(ctx, ps, merged)
                right = left
                left = 0
            } else if left + 1 < right {
                left += 1
            } else {
                right -= 1
                let token = group[right]
                if token.phonemes == nil {
                    if token.text.allSatisfy({ subtokenJunks.contains($0) }) {
                        token.phonemes = ""
                        token.rating = 3
                    } else if token.text.contains(where: \.isLetter) {
                        shouldFallback = true
                        break
                    }
                    // Anything else (the ":" in 3:45) stays silent, as in
                    // misaki without a fallback.
                }
                left = 0
            }
        }
        if shouldFallback {
            let merged = mergeTokens(group, unknown: nil)
            group[0].phonemes = fallback(merged.text) ?? ""
            for token in group.dropFirst() { token.phonemes = "" }
            ctx = tokenContext(ctx, group[0].phonemes, merged)
        } else {
            resolveTokens(group)
        }
    }

    private func resolveTokens(_ tokens: [MToken]) {
        let text = tokens.dropLast().map { $0.text + $0.whitespace }.joined() + (tokens.last?.text ?? "")
        let kinds = Set(
            text.filter { !subtokenJunks.contains($0) }.map { c -> Int in
                c.isLetter ? 0 : (c.isASCII && c.isNumber ? 1 : 2)
            })
        let prespace = text.contains(" ") || text.contains("/") || kinds.count > 1
        for (index, token) in tokens.enumerated() {
            if token.phonemes == nil {
                if index == tokens.count - 1, token.text.count == 1,
                    let c = token.text.first, nonQuotePuncts.contains(c)
                {
                    token.phonemes = token.text
                    token.rating = 3
                } else if token.text.allSatisfy({ subtokenJunks.contains($0) }) {
                    token.phonemes = ""
                    token.rating = 3
                }
            } else if index > 0 {
                token.prespace = prespace
            }
        }
        if prespace { return }
        var indices: [(Bool, Int, Int)] = []
        for (index, token) in tokens.enumerated() {
            if let ps = token.phonemes, !ps.isEmpty {
                indices.append((ps.unicodeScalars.contains(primaryStress), stressWeight(ps), index))
            }
        }
        if indices.count == 2, tokens[indices[0].2].text.count == 1 {
            let index = indices[1].2
            tokens[index].phonemes = applyStress(tokens[index].phonemes, -0.5)
            return
        } else if indices.count < 2
            || indices.filter({ $0.0 }).count <= (indices.count + 1) / 2
        {
            return
        }
        let lighter = indices.sorted { a, b in
            if a.0 != b.0 { return !a.0 }
            if a.1 != b.1 { return a.1 < b.1 }
            return a.2 < b.2
        }.prefix(indices.count / 2)
        for (_, _, index) in lighter {
            tokens[index].phonemes = applyStress(tokens[index].phonemes, -0.5)
        }
    }

    private func tokenContext(_ ctx: TokenContext, _ ps: String?, _ token: MToken) -> TokenContext {
        var vowel = ctx.futureVowel
        if let ps, !ps.isEmpty {
            for c in ps.unicodeScalars
            where vowels.contains(c) || consonants.contains(c) || nonQuotePunctScalars.contains(c) {
                vowel = nonQuotePunctScalars.contains(c) ? nil : vowels.contains(c)
                break
            }
        }
        let futureTo =
            token.text == "to" || token.text == "To" || (token.text == "TO" && (token.tag == "TO" || token.tag == "IN"))
        return TokenContext(futureVowel: vowel, futureTo: futureTo)
    }

    // MARK: Retokenizing (misaki G2P.retokenize)

    private enum Word {
        case single(MToken)
        case group([MToken])
    }

    private func retokenize(_ tokens: [MToken]) -> [Word] {
        var words: [Word] = []
        var currency: String?
        for (i, token) in tokens.enumerated() {
            var pieces: [MToken]
            if token.alias == nil && token.phonemes == nil {
                pieces = G2PTokenizer.subtokenize(token.text).map { text in
                    let piece = MToken(text: text, tag: token.tag, whitespace: "")
                    piece.stress = token.stress
                    return piece
                }
                if pieces.isEmpty { pieces = [MToken(text: token.text, tag: token.tag, whitespace: "")] }
            } else {
                pieces = [token]
            }
            pieces[pieces.count - 1].whitespace = token.whitespace
            for (j, piece) in pieces.enumerated() {
                if piece.alias != nil || piece.phonemes != nil {
                } else if piece.tag == "$" && currencies[piece.text] != nil {
                    currency = piece.text
                    piece.phonemes = ""
                    piece.rating = 4
                } else if piece.tag == ":" && (piece.text == "-" || piece.text == "–") {
                    piece.phonemes = "—"
                    piece.rating = 3
                } else if punctTags.contains(piece.tag)
                    && !piece.text.lowercased().unicodeScalars.allSatisfy({ (97...122).contains($0.value) })
                {
                    piece.phonemes =
                        punctTagPhonemes[piece.tag] ?? String(piece.text.filter { puncts.contains($0) })
                    piece.rating = 4
                } else if currency != nil {
                    if piece.tag != "CD" {
                        currency = nil
                    } else if j + 1 == pieces.count && (i + 1 == tokens.count || tokens[i + 1].tag != "CD") {
                        piece.currency = currency
                    }
                } else if j > 0, j < pieces.count - 1, piece.text == "2",
                    let before = pieces[j - 1].text.last, let after = pieces[j + 1].text.first,
                    before.isLetter, after.isLetter
                {
                    piece.alias = "to"
                }
                if piece.alias != nil || piece.phonemes != nil {
                    words.append(.single(piece))
                } else if case .group(var group)? = words.last, let last = group.last, last.whitespace.isEmpty {
                    piece.isHead = false
                    group.append(piece)
                    words[words.count - 1] = .group(group)
                } else if piece.whitespace.isEmpty {
                    words.append(.group([piece]))
                } else {
                    words.append(.single(piece))
                }
            }
        }
        return words.map { word in
            if case .group(let group) = word, group.count == 1 { return .single(group[0]) }
            return word
        }
    }

    private func mergeTokens(_ tokens: [MToken], unknown: String?) -> MToken {
        let stresses = Set(tokens.compactMap(\.stress))
        let currencies = tokens.compactMap(\.currency)
        var phonemes: String?
        if let unknown {
            var joined = ""
            for token in tokens {
                if token.prespace, let last = joined.last, !last.isWhitespace,
                    let ps = token.phonemes, !ps.isEmpty
                {
                    joined += " "
                }
                joined += token.phonemes ?? unknown
            }
            phonemes = joined
        }
        let tag =
            tokens.max { a, b in
                capsWeight(a.text) < capsWeight(b.text)
            }?.tag ?? ""
        let merged = MToken(
            text: tokens.dropLast().map { $0.text + $0.whitespace }.joined() + (tokens.last?.text ?? ""),
            tag: tag,
            whitespace: tokens.last?.whitespace ?? "")
        merged.phonemes = phonemes
        merged.isHead = tokens.first?.isHead ?? true
        merged.stress = stresses.count == 1 ? stresses.first : nil
        merged.currency = currencies.max()
        merged.prespace = tokens.first?.prespace ?? false
        return merged
    }

    /// Python's `max` keeps the first of equals; so does this.
    private func capsWeight(_ text: String) -> Int {
        text.reduce(0) { $0 + (String($1) == String($1).lowercased() ? 1 : 2) }
    }

    // MARK: Lexicon (misaki Lexicon)

    /// misaki grows each lexicon with capitalized and lowercased copies of
    /// its keys; looking those up on demand avoids building 200k entries.
    private func entry(_ word: String, in dictionary: NSDictionary) -> Any? {
        if let value = dictionary[word] { return value }
        guard word.count >= 2 else { return nil }
        let lower = word.lowercased()
        if word == capitalize(lower), lower != word, let value = dictionary[lower] {
            return value
        }
        if word == lower {
            let capitalized = capitalize(word)
            if capitalized != word, capitalized == capitalize(capitalized.lowercased()),
                let value = dictionary[capitalized]
            {
                return value
            }
        }
        return nil
    }

    private func inGolds(_ word: String) -> Bool { entry(word, in: golds) != nil }
    private func inSilvers(_ word: String) -> Bool { entry(word, in: silvers) != nil }

    private func goldString(_ word: String) -> String? { entry(word, in: golds) as? String }

    private func getNNP(_ word: String) -> (String?, Int?) {
        var parts: [String] = []
        for c in word where c.isLetter {
            guard let ps = goldString(String(c).uppercased()) else { return (nil, nil) }
            parts.append(ps)
        }
        guard let joined = applyStress(parts.joined(), 0) else { return (nil, nil) }
        var scalars = Array(joined.unicodeScalars)
        if let last = scalars.lastIndex(of: secondaryStress) {
            scalars[last] = primaryStress
        }
        return (String(String.UnicodeScalarView(scalars)), 3)
    }

    private func getSpecialCase(_ word: String, _ tag: String, _ stress: Double?, _ ctx: TokenContext?) -> (
        String?, Int?
    ) {
        if tag == "ADD", let symbol = addSymbols[word] {
            return lookup(symbol, nil, -0.5, ctx)
        } else if let symbol = symbols[word] {
            return lookup(symbol, nil, nil, ctx)
        } else if word.trimmingCharacters(in: CharacterSet(charactersIn: ".")).contains("."),
            isAlpha(word.replacingOccurrences(of: ".", with: "")),
            (word.split(separator: ".", omittingEmptySubsequences: false).map(\.count).max() ?? 0) < 3
        {
            return getNNP(word)
        } else if word == "a" || word == "A" {
            return (tag == "DT" ? "ɐ" : "ˈA", 4)
        } else if word == "am" || word == "Am" || word == "AM" {
            if tag.hasPrefix("NN") {
                return getNNP(word)
            } else if ctx?.futureVowel == nil || word != "am" || (stress ?? 0) > 0 {
                return (goldString("am"), 4)
            }
            return ("ɐm", 4)
        } else if word == "an" || word == "An" || word == "AN" {
            if word == "AN" && tag.hasPrefix("NN") { return getNNP(word) }
            return ("ɐn", 4)
        } else if word == "I" && tag == "PRP" {
            return ("ˌI", 4)
        } else if ["by", "By", "BY"].contains(word) && parentTag(tag) == "ADV" {
            return ("bˈI", 4)
        } else if word == "to" || word == "To" || (word == "TO" && (tag == "TO" || tag == "IN")) {
            switch ctx?.futureVowel {
            case nil: return (goldString("to"), 4)
            case false?: return ("tə", 4)
            case true?: return ("tʊ", 4)
            }
        } else if word == "in" || word == "In" || (word == "IN" && tag != "NNP") {
            let stress = (ctx?.futureVowel == nil || tag != "IN") ? "ˈ" : ""
            return (stress + "ɪn", 4)
        } else if word == "the" || word == "The" || (word == "THE" && tag == "DT") {
            return (ctx?.futureVowel == true ? "ði" : "ðə", 4)
        } else if tag == "IN", word.lowercased() == "vs" || word.lowercased() == "vs." {
            return lookup("versus", nil, nil, ctx)
        } else if word == "used" || word == "Used" || word == "USED" {
            let forms = entry("used", in: golds) as? NSDictionary
            if (tag == "VBD" || tag == "JJ"), ctx?.futureTo == true {
                return (forms?["VBD"] as? String, 4)
            }
            return (forms?["DEFAULT"] as? String, 4)
        }
        return (nil, nil)
    }

    private func parentTag(_ tag: String?) -> String? {
        guard let tag else { return nil }
        if tag.hasPrefix("VB") { return "VERB" }
        if tag.hasPrefix("NN") { return "NOUN" }
        if tag.hasPrefix("ADV") || tag.hasPrefix("RB") { return "ADV" }
        if tag.hasPrefix("ADJ") || tag.hasPrefix("JJ") { return "ADJ" }
        return tag
    }

    private func isKnown(_ word: String, _ tag: String) -> Bool {
        if inGolds(word) || symbols[word] != nil || inSilvers(word) {
            return true
        } else if !isAlpha(word) || !word.unicodeScalars.allSatisfy(isLexiconScalar) {
            return false
        } else if word.count == 1 {
            return true
        } else if word == word.uppercased() && inGolds(word.lowercased()) {
            return true
        }
        let rest = String(word.dropFirst())
        return rest == rest.uppercased()
    }

    private func lookup(_ word: String, _ tag: String?, _ stress: Double?, _ ctx: TokenContext?) -> (String?, Int?) {
        var word = word
        var isNNP: Bool?
        if word == word.uppercased() && !inGolds(word) {
            word = word.lowercased()
            isNNP = tag == "NNP"
        }
        var value = entry(word, in: golds)
        var rating = 4
        if value == nil && isNNP != true {
            value = entry(word, in: silvers)
            rating = 3
        }
        var ps: String?
        if let forms = value as? NSDictionary {
            var key = tag
            if ctx != nil, ctx?.futureVowel == nil, forms["None"] != nil {
                key = "None"
            } else if key == nil || forms[key!] == nil {
                key = parentTag(key)
            }
            if let key, let form = forms[key] {
                ps = form as? String
            } else {
                ps = forms["DEFAULT"] as? String
            }
        } else {
            ps = value as? String
        }
        if ps == nil || (isNNP == true && !(ps!.unicodeScalars.contains(primaryStress))) {
            let (nnp, nnpRating) = getNNP(word)
            if let nnp { return (nnp, nnpRating) }
        }
        return (applyStress(ps, stress), rating)
    }

    private func suffixS(_ stem: String?) -> String? {
        guard let stem, let last = stem.unicodeScalars.last else { return nil }
        if "ptkfθ".unicodeScalars.contains(last) { return stem + "s" }
        if "szʃʒʧʤ".unicodeScalars.contains(last) { return stem + (british ? "ɪ" : "ᵻ") + "z" }
        return stem + "z"
    }

    private func stemS(_ word: String, _ tag: String, _ stress: Double?, _ ctx: TokenContext?) -> (String?, Int?) {
        guard word.count >= 3, word.hasSuffix("s") else { return (nil, nil) }
        let stem: String
        if !word.hasSuffix("ss") && isKnown(String(word.dropLast()), tag) {
            stem = String(word.dropLast())
        } else if (word.hasSuffix("'s") || (word.count > 4 && word.hasSuffix("es") && !word.hasSuffix("ies")))
            && isKnown(String(word.dropLast(2)), tag)
        {
            stem = String(word.dropLast(2))
        } else if word.count > 4 && word.hasSuffix("ies") && isKnown(String(word.dropLast(3)) + "y", tag) {
            stem = String(word.dropLast(3)) + "y"
        } else {
            return (nil, nil)
        }
        let (ps, rating) = lookup(stem, tag, stress, ctx)
        return (suffixS(ps), rating)
    }

    private func suffixED(_ stem: String?) -> String? {
        guard let stem else { return nil }
        let scalars = Array(stem.unicodeScalars)
        guard let last = scalars.last else { return nil }
        if "pkfθʃsʧ".unicodeScalars.contains(last) { return stem + "t" }
        if last == "d" { return stem + (british ? "ɪ" : "ᵻ") + "d" }
        if last != "t" { return stem + "d" }
        if british || scalars.count < 2 { return stem + "ɪd" }
        if usTaus.contains(scalars[scalars.count - 2]) {
            return String(String.UnicodeScalarView(scalars.dropLast())) + "ɾᵻd"
        }
        return stem + "ᵻd"
    }

    private func stemED(_ word: String, _ tag: String, _ stress: Double?, _ ctx: TokenContext?) -> (String?, Int?) {
        guard word.count >= 4, word.hasSuffix("d") else { return (nil, nil) }
        let stem: String
        if !word.hasSuffix("dd") && isKnown(String(word.dropLast()), tag) {
            stem = String(word.dropLast())
        } else if word.count > 4 && word.hasSuffix("ed") && !word.hasSuffix("eed")
            && isKnown(String(word.dropLast(2)), tag)
        {
            stem = String(word.dropLast(2))
        } else {
            return (nil, nil)
        }
        let (ps, rating) = lookup(stem, tag, stress, ctx)
        return (suffixED(ps), rating)
    }

    private func suffixING(_ stem: String?) -> String? {
        guard let stem else { return nil }
        let scalars = Array(stem.unicodeScalars)
        guard let last = scalars.last else { return nil }
        if british {
            if last == "ə" || last == "ː" { return nil }
        } else if scalars.count > 1 && last == "t" && usTaus.contains(scalars[scalars.count - 2]) {
            return String(String.UnicodeScalarView(scalars.dropLast())) + "ɾɪŋ"
        }
        return stem + "ɪŋ"
    }

    private func stemING(_ word: String, _ tag: String, _ stress: Double?, _ ctx: TokenContext?) -> (String?, Int?) {
        guard word.count >= 5, word.hasSuffix("ing") else { return (nil, nil) }
        let base = String(word.dropLast(3))
        let stem: String
        if word.count > 5 && isKnown(base, tag) {
            stem = base
        } else if isKnown(base + "e", tag) {
            stem = base + "e"
        } else if word.count > 5, doubledConsonantING(word), isKnown(String(word.dropLast(4)), tag) {
            stem = String(word.dropLast(4))
        } else {
            return (nil, nil)
        }
        let (ps, rating) = lookup(stem, tag, stress, ctx)
        return (suffixING(ps), rating)
    }

    /// `([bcdgklmnprstvxz])\1ing$|cking$`
    private func doubledConsonantING(_ word: String) -> Bool {
        if word.hasSuffix("cking") { return true }
        let chars = Array(word)
        guard chars.count >= 5 else { return false }
        let a = chars[chars.count - 5]
        let b = chars[chars.count - 4]
        return a == b && "bcdgklmnprstvxz".contains(a)
    }

    private func getWord(_ word: String, _ tag: String, _ stress: Double?, _ ctx: TokenContext?) -> (String?, Int?) {
        let special = getSpecialCase(word, tag, stress, ctx)
        if special.0 != nil { return special }
        var word = word
        let wl = word.lowercased()
        let rest = String(word.dropFirst())
        if word.count > 1, isAlpha(word.replacingOccurrences(of: "'", with: "")), word != wl,
            tag != "NNP" || word.count > 7, !inGolds(word), !inSilvers(word),
            word == word.uppercased() || rest == rest.lowercased(),
            inGolds(wl) || inSilvers(wl) || stemS(wl, tag, stress, ctx).0 != nil
                || stemED(wl, tag, stress, ctx).0 != nil || stemING(wl, tag, stress, ctx).0 != nil
        {
            word = wl
        }
        if isKnown(word, tag) {
            return lookup(word, tag, stress, ctx)
        } else if word.hasSuffix("s'") && isKnown(String(word.dropLast(2)) + "'s", tag) {
            return lookup(String(word.dropLast(2)) + "'s", tag, stress, ctx)
        } else if word.hasSuffix("'") && isKnown(String(word.dropLast()), tag) {
            return lookup(String(word.dropLast()), tag, stress, ctx)
        }
        let s = stemS(word, tag, stress, ctx)
        if s.0 != nil { return s }
        let ed = stemED(word, tag, stress, ctx)
        if ed.0 != nil { return ed }
        let ing = stemING(word, tag, stress ?? 0.5, ctx)
        if ing.0 != nil { return ing }
        return (nil, nil)
    }

    private func appendCurrency(_ ps: String, _ currency: String?) -> String {
        guard let currency, let units = currencies[currency] else { return ps }
        guard let plural = stemS(units.0 + "s", "", nil, nil).0 else { return ps }
        return "\(ps) \(plural)"
    }

    /// misaki Lexicon.__call__
    private func lexicon(_ token: MToken, _ ctx: TokenContext?) -> (String?, Int?) {
        var word = (token.alias ?? token.text)
            .replacingOccurrences(of: "\u{2018}", with: "'")
            .replacingOccurrences(of: "\u{2019}", with: "'")
        word = word.precomposedStringWithCompatibilityMapping
        word = String(word.map { c -> String in
            if c.isNumber, !c.isASCII, let value = c.wholeNumberValue { return String(value) }
            return String(c)
        }.joined())
        let stress: Double? = word == word.lowercased() ? nil : (word == word.uppercased() ? 2 : 0.5)
        let (ps, rating) = getWord(word, token.tag, stress, ctx)
        if let ps {
            return (applyStress(appendCurrency(ps, token.currency), token.stress), rating)
        } else if NumberReader.isNumber(word, isHead: token.isHead) {
            let (number, numberRating) = getNumber(word, token.currency, token.isHead)
            return (applyStress(number, token.stress), numberRating)
        }
        return (nil, nil)
    }

    // MARK: Numbers (misaki Lexicon.get_number, with our own number words)

    private func getNumber(_ word: String, _ currency: String?, _ isHead: Bool) -> (String?, Int?) {
        var word = word
        var suffix: String?
        if let range = word.range(of: "[a-z']+$", options: .regularExpression) {
            suffix = String(word[range])
            word.removeSubrange(range)
        }
        var result: [(String?, Int?)] = []
        if word.hasPrefix("-") {
            result.append(lookup("minus", nil, nil, nil))
            word.removeFirst()
        }
        func extend(_ words: [String]) {
            for w in words where w != "and" && !w.isEmpty {
                result.append(lookup(w, nil, w == "point" ? -2 : nil, nil))
            }
        }
        func extend(number: String) {
            extend(NumberReader.cardinal(UInt64(number) ?? 0))
        }
        let digits = !word.isEmpty && word.allSatisfy { $0.isASCII && $0.isNumber }
        if digits, let suffix, ordinalSuffixes.contains(suffix) {
            extend(NumberReader.ordinal(UInt64(word) ?? 0))
        } else if result.isEmpty, word.count == 4, currencies[currency ?? ""] == nil, digits {
            extend(NumberReader.year(Int(word) ?? 0))
        } else if !isHead && !word.contains(".") {
            let number = word.replacingOccurrences(of: ",", with: "")
            if number.hasPrefix("0") || number.count > 3 {
                for digit in number { extend(number: String(digit)) }
            } else if number.count == 3 && !number.hasSuffix("00") {
                let chars = Array(number)
                extend(number: String(chars[0]))
                if chars[1] == "0" {
                    result.append(lookup("O", nil, -2, nil))
                    extend(number: String(chars[2]))
                } else {
                    extend(number: String(chars[1...]))
                }
            } else {
                extend(number: number)
            }
        } else if word.filter({ $0 == "." }).count > 1 || !isHead {
            for part in word.replacingOccurrences(of: ",", with: "").split(separator: ".", omittingEmptySubsequences: true) {
                let part = String(part)
                if part.hasPrefix("0") || (part.count != 2 && part.dropFirst().contains { $0 != "0" }) {
                    for digit in part { extend(number: String(digit)) }
                } else {
                    extend(number: part)
                }
            }
        } else if let currency, let units = currencies[currency], NumberReader.isCurrency(word) {
            let parts = word.replacingOccurrences(of: ",", with: "").split(separator: ".", omittingEmptySubsequences: false)
            var pairs: [(UInt64, String)] = zip(parts, [units.0, units.1]).map { (UInt64($0.0) ?? 0, $0.1) }
            if pairs.count > 1 {
                if pairs[1].0 == 0 {
                    pairs = [pairs[0]]
                } else if pairs[0].0 == 0 {
                    pairs = [pairs[1]]
                }
            }
            for (index, (number, unit)) in pairs.enumerated() {
                if index > 0 { result.append(lookup("and", nil, nil, nil)) }
                extend(NumberReader.cardinal(number))
                if number != 1 && unit != "pence" {
                    result.append(stemS(unit + "s", "", nil, nil))
                } else {
                    result.append(lookup(unit, nil, nil, nil))
                }
            }
        } else {
            if digits {
                extend(NumberReader.cardinal(UInt64(word) ?? 0))
            } else if !word.contains(".") {
                let value = UInt64(word.replacingOccurrences(of: ",", with: "")) ?? 0
                extend(
                    suffix.map(ordinalSuffixes.contains) == true
                        ? NumberReader.ordinal(value) : NumberReader.cardinal(value))
            } else {
                extend(NumberReader.decimal(word.replacingOccurrences(of: ",", with: "")))
            }
        }
        guard !result.isEmpty, result.allSatisfy({ $0.0 != nil }) else { return (nil, nil) }
        let joined = result.compactMap(\.0).joined(separator: " ")
        let rating = result.compactMap(\.1).min()
        switch suffix {
        case "s", "'s": return (suffixS(joined), rating)
        case "ed", "'d": return (suffixED(joined), rating)
        case "ing": return (suffixING(joined), rating)
        default: return (joined, rating)
        }
    }

    // MARK: Words missing from the lexicons

    /// Pronounces what the lexicons do not know: acronyms letter by letter,
    /// compounds of two known words, and otherwise letter-to-sound rules.
    private func fallback(_ text: String) -> String? {
        let word = text.replacingOccurrences(of: "\u{2019}", with: "'")
            .folding(options: .diacriticInsensitive, locale: nil)
        var parts: [String] = []
        var current = ""
        var separated = false
        func flush() {
            guard !current.isEmpty else { return }
            if let ps = fallbackPart(current) {
                if separated, !parts.isEmpty { parts.append(" ") }
                parts.append(ps)
            }
            current = ""
            separated = false
        }
        for c in word {
            if c.isLetter || c == "'" {
                current.append(c)
            } else {
                flush()
                separated = true
            }
        }
        flush()
        let joined = parts.joined()
        return joined.isEmpty ? nil : joined
    }

    private func fallbackPart(_ word: String) -> String? {
        var word = word.trimmingCharacters(in: CharacterSet(charactersIn: "'"))
        guard !word.isEmpty else { return nil }
        var possessive = false
        if word.lowercased().hasSuffix("'s") {
            possessive = true
            word = String(word.dropLast(2))
        }
        word = word.replacingOccurrences(of: "'", with: "")
        guard !word.isEmpty else { return nil }
        var ps: String?
        let letters = word.filter(\.isLetter)
        let vowelless = !letters.lowercased().contains { "aeiouy".contains($0) }
        if (letters.count <= 5 && letters == letters.uppercased() && (letters.count > 1 || word.count == 1))
            || (vowelless && letters.count <= 6)
        {
            // Acronyms and vowelless names (pnpm) are spelled out.
            ps = getNNP(letters).0
        }
        if ps == nil, let known = getWord(word, "", nil, nil).0 {
            ps = known
        }
        if ps == nil { ps = compound(word.lowercased()) }
        if ps == nil { ps = letterToSound.phonemes(word.lowercased(), british: british) }
        return possessive ? suffixS(ps) : ps
    }

    /// Two known words ("voicemail", "dashboard"), stressed on the first.
    private func compound(_ word: String) -> String? {
        let chars = Array(word)
        guard chars.count >= 6 else { return nil }
        var best: (String, String, Int)?
        for split in 3...(chars.count - 3) {
            let head = String(chars[..<split])
            let tail = String(chars[split...])
            guard inGolds(head) || inSilvers(head), inGolds(tail) || inSilvers(tail) else { continue }
            let shorter = min(head.count, tail.count)
            if best == nil || shorter > best!.2 { best = (head, tail, shorter) }
        }
        guard let (head, tail, _) = best,
            let first = lookup(head, nil, nil, nil).0,
            let second = lookup(tail, nil, nil, nil).0
        else { return nil }
        return first + (applyStress(second, -1) ?? second)
    }
}

// MARK: - Tokens

private final class MToken {
    var text: String
    var tag: String
    var whitespace: String
    var phonemes: String?
    var rating: Int?
    var isHead = true
    var alias: String?
    var stress: Double?
    var currency: String?
    var prespace = false

    init(text: String, tag: String, whitespace: String) {
        self.text = text
        self.tag = tag
        self.whitespace = whitespace
    }
}

private struct TokenContext {
    /// Whether the next spoken word starts with a vowel; nil when unknown or
    /// after punctuation.
    var futureVowel: Bool?
    var futureTo = false
}

// MARK: - Tokenizer standing in for spaCy

private enum G2PTokenizer {
    static let linkRegex = try! NSRegularExpression(pattern: #"\[([^\]]+)\]\(([^\)]*)\)"#)
    static let subtokenRegex = try! NSRegularExpression(
        pattern:
            #"^['‘’]+|\p{Lu}(?=\p{Lu}\p{Ll})|(?:^-)?(?:\d?[,.]?\d)+|[-_]+|['‘’]{2,}|\p{L}*?(?:['‘’]\p{L})*?\p{Ll}(?=\p{Lu})|\p{L}+(?:['‘’]\p{L})*|[^-_\p{L}'‘’\d]|['‘’]+$"#
    )

    /// Markdown links read as their text; whitespace collapses to spaces.
    static func preprocess(_ text: String) -> String {
        let range = NSRange(text.startIndex..., in: text)
        let unlinked = linkRegex.stringByReplacingMatches(in: text, range: range, withTemplate: "$1")
        return unlinked.split(whereSeparator: \.isWhitespace).joined(separator: " ")
    }

    static func subtokenize(_ word: String) -> [String] {
        let range = NSRange(word.startIndex..., in: word)
        return subtokenRegex.matches(in: word, range: range).compactMap {
            Range($0.range, in: word).map { String(word[$0]) }
        }
    }

    private static let prefixes: Set<Character> = ["\"", "“", "‘", "(", "[", "{", "$", "£", "€", "#", "*", "¿", "¡"]
    private static let suffixes: Set<Character> = [
        "\"", "”", ")", "]", "}", ".", ",", ";", ":", "!", "?", "…", "*", "%",
    ]
    private static let infixes: Set<Character> = ["—", "–", "…"]

    static func tokenize(_ text: String) -> [MToken] {
        var tokens: [MToken] = []
        let chunks = text.split(separator: " ")
        for chunk in chunks {
            let pieces = split(String(chunk))
            if pieces.isEmpty { continue }
            pieces[pieces.count - 1].whitespace = " "
            tokens.append(contentsOf: pieces)
        }
        tokens.last?.whitespace = ""
        tag(tokens)
        return tokens
    }

    /// One whitespace-separated chunk, split the way spaCy splits off
    /// punctuation: prefixes, suffixes and dashes inside words.
    private static func split(_ chunk: String) -> [MToken] {
        if ["-", "–", "—", "--"].contains(chunk) {
            return [MToken(text: chunk == "—" ? "—" : "-", tag: ":", whitespace: "")]
        }
        var chars = Array(chunk)
        if chars.count == 1, let only = chars.first, prefixes.contains(only) || suffixes.contains(only) {
            return [MToken(text: chunk, tag: punctTag(only, opening: true), whitespace: "")]
        }
        var head: [MToken] = []
        var tail: [MToken] = []
        while let first = chars.first, prefixes.contains(first), chars.count > 1 {
            head.append(MToken(text: String(first), tag: punctTag(first, opening: true), whitespace: ""))
            chars.removeFirst()
        }
        let abbreviation = isAbbreviation(String(chars))
        while let last = chars.last, suffixes.contains(last) {
            if last == "." && abbreviation { break }
            if last == "." {
                var count = 0
                while let c = chars.last, c == "." {
                    chars.removeLast()
                    count += 1
                }
                tail.insert(MToken(text: String(repeating: ".", count: count), tag: count > 1 ? ":" : ".", whitespace: ""), at: 0)
                continue
            }
            chars.removeLast()
            tail.insert(MToken(text: String(last), tag: punctTag(last, opening: false), whitespace: ""), at: 0)
        }
        var middle: [MToken] = []
        var current = ""
        for c in chars {
            if infixes.contains(c) {
                if !current.isEmpty { middle.append(MToken(text: current, tag: "", whitespace: "")) }
                middle.append(MToken(text: c == "–" ? "-" : String(c), tag: ":", whitespace: ""))
                current = ""
            } else {
                current.append(c)
            }
        }
        if !current.isEmpty { middle.append(MToken(text: current, tag: "", whitespace: "")) }
        return head + middle + tail
    }

    private static func isAbbreviation(_ text: String) -> Bool {
        text.range(of: #"^([A-Za-z]\.){2,}$"#, options: .regularExpression) != nil
            || ["e.g.", "i.e.", "etc.", "vs.", "Mr.", "Mrs.", "Ms.", "Dr.", "St."].contains(text)
    }

    private static func punctTag(_ c: Character, opening: Bool) -> String {
        switch c {
        case ".", "!", "?", "¿", "¡": return "."
        case ",": return ","
        case ";", ":", "…", "—", "–": return ":"
        case "\"": return opening ? "``" : "''"
        case "“", "‘": return "``"
        case "”": return "''"
        case "(", "[", "{": return "-LRB-"
        case ")", "]", "}": return "-RRB-"
        case "$", "£", "€", "#": return "$"
        case "%": return "NN"
        default: return "NFP"
        }
    }

    private static let determiners: Set<String> = [
        "the", "a", "an", "this", "these", "those", "my", "your", "our", "their", "his", "her", "its", "each",
        "every", "no", "some", "any", "new", "old", "first", "last", "next", "one", "another",
    ]
    private static let subjects: Set<String> = ["i", "you", "we", "they", "he", "she", "it", "who"]
    private static let modals: Set<String> = [
        "to", "will", "would", "can", "could", "should", "might", "must", "may", "shall", "do", "does", "did",
        "don't", "doesn't", "didn't", "won't", "can't", "cannot", "let's", "please", "let", "i'll", "we'll",
        "you'll", "i'd", "we'd",
    ]
    private static let perfect: Set<String> = [
        "have", "has", "had", "'ve", "was", "were", "been", "be", "being", "is", "are", "am", "got", "get",
        "i've", "we've", "you've", "they've", "it's", "that's", "isn't", "wasn't", "aren't", "weren't",
        "haven't", "hasn't",
    ]
    private static let prepositions: Set<String> = [
        "in", "on", "at", "by", "for", "with", "from", "of", "about", "into", "onto", "over", "after", "before",
        "under", "between", "through", "during", "without", "within", "than", "as", "if", "because", "since",
        "while", "until", "against", "across", "per", "via", "like", "that",
    ]
    private static let pronouns: Set<String> = [
        "i", "you", "he", "she", "it", "we", "they", "me", "him", "us", "them",
    ]

    private static let copulas: Set<String> = [
        "is", "are", "was", "were", "be", "been", "being", "am", "it's", "that's", "isn't", "wasn't",
        "aren't", "weren't", "looks", "seems", "feels", "stays", "remains",
    ]
    private static let auxiliaries: Set<String> = [
        "have", "has", "had", "'ve", "i've", "we've", "you've", "they've", "haven't", "hasn't", "hadn't",
        "got", "get",
    ]
    private static let adverbs: Set<String> = [
        "now", "also", "then", "just", "still", "never", "always", "often", "only", "really", "actually", "so",
        "please", "finally", "simply", "already",
    ]

    /// Penn tags guessed from neighbours: enough to choose between the
    /// lexicon's NOUN/VERB/VBD/ADJ pronunciations and misaki's special cases.
    private static func tag(_ tokens: [MToken]) {
        var sentenceStart = true
        var previous: String?
        var previousIsName = false
        var previousTag = ""
        for (index, token) in tokens.enumerated() {
            if !token.tag.isEmpty {
                if token.tag == "." || token.tag == "," || token.tag == ":" { sentenceStart = token.tag == "." }
                if token.tag != "``" && token.tag != "-LRB-" && token.tag != "$" {
                    previous = nil
                    previousIsName = false
                    previousTag = token.tag
                }
                continue
            }
            let word = token.text
            let lower = word.lowercased()
            let nextIsPunct = index + 1 >= tokens.count || !tokens[index + 1].tag.isEmpty
            let next = nextIsPunct ? nil : tokens[index + 1].text.lowercased()
            let capitalized = word.first?.isUppercase == true && word != word.uppercased()
            let nextIsDeterminer = next.map { determiners.contains($0) || $0 == "the" } ?? false
            if word.contains(where: \.isNumber) && NumberReader.isNumber(word, isHead: true) {
                token.tag = "CD"
            } else if lower == "that's" {
                token.tag = "DT"
            } else if lower == "that" {
                if let previous, prepositions.contains(previous) || previous == "to" {
                    token.tag = "DT"
                } else if previousTag.hasPrefix("NN"), !nextIsPunct {
                    // "a script that downloads": a relative pronoun.
                    token.tag = "WDT"
                } else if let next, determiners.contains(next) || subjects.contains(next) || pronouns.contains(next)
                    || next == "the"
                {
                    token.tag = "IN"
                } else {
                    token.tag = "DT"
                }
            } else if ["the", "a", "an", "this", "these", "those", "each", "every", "no"].contains(lower) {
                token.tag = "DT"
            } else if word == "I" || (pronouns.contains(lower) && lower != "i") {
                token.tag = "PRP"
            } else if lower == "to" {
                token.tag = "TO"
            } else if prepositions.contains(lower) {
                token.tag = "IN"
            } else if !sentenceStart && capitalized {
                token.tag = "NNP"
            } else if let previous, modals.contains(previous) || adverbs.contains(previous) {
                token.tag = "VB"
            } else if let previous, auxiliaries.contains(previous) {
                token.tag = lower.hasSuffix("ing") ? "VBG" : "VBN"
            } else if let previous, copulas.contains(previous) {
                token.tag = lower.hasSuffix("ing") ? "VBG" : (lower.hasSuffix("ed") ? "VBN" : "JJ")
            } else if let previous, determiners.contains(previous) {
                token.tag = "NN"
            } else if let previous, subjects.contains(previous) || previousIsName {
                token.tag = "VBD"
            } else if nextIsDeterminer {
                token.tag = "VB"
            } else if lower.hasSuffix("ed") {
                token.tag = "VBD"
            } else if lower.hasSuffix("ly") {
                token.tag = "RB"
            } else {
                token.tag = "NN"
            }
            previous = lower
            previousTag = token.tag
            previousIsName = token.tag == "NNP" || (sentenceStart && capitalized && token.tag == "NN")
            sentenceStart = false
        }
    }
}

// MARK: - Numbers

/// English number words, written for this port (misaki uses num2words).
enum NumberReader {
    private static let ones = [
        "zero", "one", "two", "three", "four", "five", "six", "seven", "eight", "nine", "ten", "eleven", "twelve",
        "thirteen", "fourteen", "fifteen", "sixteen", "seventeen", "eighteen", "nineteen",
    ]
    private static let tens = ["", "", "twenty", "thirty", "forty", "fifty", "sixty", "seventy", "eighty", "ninety"]
    private static let scales: [(UInt64, String)] = [
        (1_000_000_000_000_000_000, "quintillion"), (1_000_000_000_000_000, "quadrillion"),
        (1_000_000_000_000, "trillion"), (1_000_000_000, "billion"), (1_000_000, "million"), (1000, "thousand"),
    ]

    static func cardinal(_ n: UInt64) -> [String] {
        if n < 20 { return [ones[Int(n)]] }
        if n < 100 {
            let unit = n % 10
            return unit == 0 ? [tens[Int(n / 10)]] : [tens[Int(n / 10)], ones[Int(unit)]]
        }
        if n < 1000 {
            let rest = n % 100
            return [ones[Int(n / 100)], "hundred"] + (rest == 0 ? [] : cardinal(rest))
        }
        for (scale, name) in scales where n >= scale {
            let rest = n % scale
            return cardinal(n / scale) + [name] + (rest == 0 ? [] : cardinal(rest))
        }
        return []
    }

    static func ordinal(_ n: UInt64) -> [String] {
        var words = cardinal(n)
        guard let last = words.popLast() else { return [] }
        let irregular = [
            "one": "first", "two": "second", "three": "third", "five": "fifth", "eight": "eighth",
            "nine": "ninth", "twelve": "twelfth",
        ]
        if let word = irregular[last] {
            words.append(word)
        } else if last.hasSuffix("y") {
            words.append(String(last.dropLast()) + "ieth")
        } else {
            words.append(last + "th")
        }
        return words
    }

    /// num2words' `to='year'`: 1999 nineteen ninety-nine, 1905 nineteen oh
    /// five, 2005 two thousand five, 2026 twenty twenty-six.
    static func year(_ n: Int) -> [String] {
        let high = n / 100
        let low = n % 100
        if high == 0 || (high % 10 == 0 && low < 10) || high >= 100 {
            return cardinal(UInt64(n))
        }
        let lowWords: [String] =
            low == 0 ? ["hundred"] : (low < 10 ? ["oh"] + cardinal(UInt64(low)) : cardinal(UInt64(low)))
        return cardinal(UInt64(high)) + lowWords
    }

    /// "3.14" three point one four; "2.50" two point five (num2words reads
    /// Python's float, which drops trailing zeros); ".5" point five.
    static func decimal(_ text: String) -> [String] {
        let parts = text.split(separator: ".", omittingEmptySubsequences: false)
        let whole = parts.first.map(String.init) ?? ""
        var fraction = parts.count > 1 ? String(parts[1]) : ""
        if text.hasPrefix(".") {
            return ["point"] + fraction.flatMap { cardinal(UInt64(String($0)) ?? 0) }
        }
        while fraction.hasSuffix("0") { fraction.removeLast() }
        if fraction.isEmpty { fraction = "0" }
        return cardinal(UInt64(whole) ?? 0) + ["point"] + fraction.flatMap { cardinal(UInt64(String($0)) ?? 0) }
    }

    static func isCurrency(_ word: String) -> Bool {
        guard word.contains(".") else { return true }
        let parts = word.split(separator: ".", omittingEmptySubsequences: false)
        guard parts.count == 2 else { return false }
        return parts[1].count < 3
    }

    static func isNumber(_ word: String, isHead: Bool) -> Bool {
        guard word.contains(where: { $0.isASCII && $0.isNumber }) else { return false }
        var word = word
        for suffix in ["ing", "'d", "ed", "'s", "st", "nd", "rd", "th", "s"] where word.hasSuffix(suffix) {
            word = String(word.dropLast(suffix.count))
            break
        }
        return word.enumerated().allSatisfy { index, c in
            (c.isASCII && c.isNumber) || c == "," || c == "." || (isHead && index == 0 && c == "-")
        }
    }
}

// MARK: - Shared tables

private let primaryStress: Unicode.Scalar = "ˈ"
private let secondaryStress: Unicode.Scalar = "ˌ"
private let stressMarks: Set<Unicode.Scalar> = [primaryStress, secondaryStress]
private let vowels = Set("AIOQWYaiuæɑɒɔəɛɜɪʊʌᵻ".unicodeScalars)
private let consonants = Set("bdfhjklmnpstvwzðŋɡɹɾʃʒʤʧθ".unicodeScalars)
private let diphthongs = Set("AIOQWYʤʧ".unicodeScalars)
private let usTaus = Set("AIOWYiuæɑəɛɪɹʊʌ".unicodeScalars)
private let puncts: Set<Character> = [";", ":", ",", ".", "!", "?", "—", "…", "\"", "“", "”"]
private let nonQuotePuncts: Set<Character> = [";", ":", ",", ".", "!", "?", "—", "…"]
private let nonQuotePunctScalars = Set(";:,.!?—…".unicodeScalars)
private let subtokenJunks: Set<Character> = ["'", ",", "-", ".", "_", "‘", "’", "/"]
private let punctTags: Set<String> = [".", ",", "-LRB-", "-RRB-", "``", "\"\"", "''", ":", "$", "#", "NFP"]
private let punctTagPhonemes = ["-LRB-": "(", "-RRB-": ")", "``": "\u{201C}", "\"\"": "\u{201D}", "''": "\u{201D}"]
private let currencies = ["$": ("dollar", "cent"), "£": ("pound", "pence"), "€": ("euro", "cent")]
private let ordinalSuffixes: Set<String> = ["st", "nd", "rd", "th"]
private let addSymbols = [".": "dot", "/": "slash"]
private let symbols = ["%": "percent", "&": "and", "+": "plus", "@": "at"]
private let kokoroVocabulary = Set(
    "$;:,.!?—…\"()“” ̃ʣʥʦʨᵝꭧAIOQSTWYᵊabcdefhijklmnopqrstuvwxyzɑɐɒæβɔɕçɖðʤəɚɛɜɟɡɥɨɪʝɯɰŋɳɲɴøɸθœɹɾɻʁɽʂʃʈʧʊʋʌɣɤχʎʒʔˈˌːʰʲ↓→↗↘ᵻ"
        .unicodeScalars)

private func isLexiconScalar(_ scalar: Unicode.Scalar) -> Bool {
    scalar.value == 39 || scalar.value == 45 || (65...90).contains(scalar.value) || (97...122).contains(scalar.value)
}

private func isAlpha(_ text: String) -> Bool {
    !text.isEmpty && text.allSatisfy(\.isLetter)
}

/// Python's str.capitalize: first character upper, the rest lower.
private func capitalize(_ text: String) -> String {
    guard let first = text.first else { return text }
    return String(first).uppercased() + text.dropFirst().lowercased()
}

private func stressWeight(_ ps: String) -> Int {
    ps.unicodeScalars.reduce(0) { $0 + (diphthongs.contains($1) ? 2 : 1) }
}

/// misaki apply_stress: -2 strips stress, -1 demotes, 0…1 adds secondary,
/// above 1 adds primary.
private func applyStress(_ ps: String?, _ stress: Double?) -> String? {
    guard let ps else { return nil }
    guard let stress else { return ps }
    let scalars = ps.unicodeScalars
    let hasPrimary = scalars.contains(primaryStress)
    let hasSecondary = scalars.contains(secondaryStress)
    func restress(_ text: String) -> String {
        let chars = Array(text.unicodeScalars)
        var items = chars.enumerated().map { (Double($0.offset), $0.element) }
        for (i, c) in chars.enumerated() where stressMarks.contains(c) {
            if let j = chars[i...].firstIndex(where: { vowels.contains($0) }) {
                items[i] = (Double(j) - 0.5, c)
            }
        }
        items.sort { a, b in a.0 != b.0 ? a.0 < b.0 : a.1.value < b.1.value }
        return String(String.UnicodeScalarView(items.map(\.1)))
    }
    if stress < -1 {
        return String(String.UnicodeScalarView(scalars.filter { !stressMarks.contains($0) }))
    } else if stress == -1 || ((stress == 0 || stress == -0.5) && hasPrimary) {
        return String(
            String.UnicodeScalarView(
                scalars.filter { $0 != secondaryStress }.map { $0 == primaryStress ? secondaryStress : $0 }))
    } else if [0, 0.5, 1].contains(stress) && !hasPrimary && !hasSecondary {
        if !scalars.contains(where: { vowels.contains($0) }) { return ps }
        return restress(String(secondaryStress) + ps)
    } else if stress >= 1 && !hasPrimary && hasSecondary {
        return String(String.UnicodeScalarView(scalars.map { $0 == secondaryStress ? primaryStress : $0 }))
    } else if stress > 1 && !hasPrimary && !hasSecondary {
        if !scalars.contains(where: { vowels.contains($0) }) { return ps }
        return restress(String(primaryStress) + ps)
    }
    return ps
}

// MARK: - Command line

/// `diri-voice phonemes [--british] --lexicon DIR` : reads stdin, prints phonemes. DIR holds us_gold.json etc.
func runPhonemes(_ arguments: [String]) {
    let british = arguments.contains("--british")
    guard let index = arguments.firstIndex(of: "--lexicon"), arguments.indices.contains(index + 1) else {
        FileHandle.standardError.write(Data("diri-voice: phonemes needs --lexicon DIR\n".utf8))
        exit(64)
    }
    let directory = URL(fileURLWithPath: arguments[index + 1])
    let prefix = british ? "gb" : "us"
    do {
        let g2p = try EnglishG2P(
            gold: Data(contentsOf: directory.appendingPathComponent("\(prefix)_gold.json")),
            silver: Data(contentsOf: directory.appendingPathComponent("\(prefix)_silver.json")),
            british: british)
        let input = String(decoding: FileHandle.standardInput.readDataToEndOfFile(), as: UTF8.self)
        for line in input.split(separator: "\n", omittingEmptySubsequences: false) {
            print(g2p.phonemes(String(line)))
        }
    } catch {
        FileHandle.standardError.write(Data("diri-voice: could not read lexicons: \(error)\n".utf8))
        exit(1)
    }
}
