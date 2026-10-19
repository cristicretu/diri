// The onboarding the computer-use clip's agents test: a small SwiftUI app
// that opens on the screen named by `-state`, so each step can be
// screenshotted on a real Simulator (see site-clip-phones.sh).
import SwiftUI

@main
struct AcmeApp: App {
    var body: some Scene {
        WindowGroup { Onboarding(step: Step.current) }
    }
}

enum Step {
    case signIn(typed: String)
    case welcome(fixed: Bool)
    case tour(page: Int)
    case notifications(alert: Bool, on: Bool)

    static var current: Step {
        let args = ProcessInfo.processInfo.arguments
        let value = args.firstIndex(of: "-state").map { args[$0 + 1] } ?? "signin"
        let parts = value.split(separator: ":", maxSplits: 1).map(String.init)
        let arg = parts.count > 1 ? parts[1] : ""
        switch parts[0] {
        case "welcome": return .welcome(fixed: arg == "fixed")
        case "tour": return .tour(page: Int(arg) ?? 0)
        case "notify": return .notifications(alert: arg == "alert", on: arg == "on")
        default: return .signIn(typed: String("sam@acme.dev".prefix(Int(arg) ?? 0)))
        }
    }
}

struct Onboarding: View {
    let step: Step

    var body: some View {
        switch step {
        case .signIn(let typed): SignIn(typed: typed)
        case .welcome(let fixed): Tour(page: 0, fixed: fixed)
        case .tour(let page): Tour(page: page, fixed: true)
        case .notifications(let alert, let on): Notifications(alert: alert, on: on)
        }
    }
}

struct Mark: View {
    var body: some View {
        RoundedRectangle(cornerRadius: 18, style: .continuous)
            .fill(LinearGradient(colors: [.indigo, .purple], startPoint: .topLeading, endPoint: .bottomTrailing))
            .frame(width: 72, height: 72)
            .overlay(Image(systemName: "sparkles").font(.system(size: 32, weight: .semibold)).foregroundStyle(.white))
    }
}

struct SignIn: View {
    let typed: String

    var body: some View {
        VStack(spacing: 0) {
            Spacer()
            Mark()
            Text("Welcome to Acme").font(.title.bold()).padding(.top, 20)
            Text("Sign in with your work email.").foregroundStyle(.secondary).padding(.top, 6)
            Spacer()
            VStack(spacing: 12) {
                HStack {
                    Text(typed.isEmpty ? "Email" : typed)
                        .foregroundStyle(typed.isEmpty ? .tertiary : .primary)
                    Spacer()
                }
                .padding(.horizontal, 16)
                .frame(height: 50)
                .background(Color(.secondarySystemBackground), in: RoundedRectangle(cornerRadius: 12, style: .continuous))
                Button {} label: { Text("Continue").frame(maxWidth: .infinity) }
                    .buttonStyle(.borderedProminent)
                    .controlSize(.large)
            }
            .padding(.horizontal, 24)
            .padding(.bottom, 24)
        }
    }
}

struct Tour: View {
    let page: Int
    let fixed: Bool

    static let pages: [(String, String, String)] = [
        ("rectangle.stack", "Your workspace", "Projects, notes and runs, in one place."),
        ("calendar.badge.clock", "Runs on a schedule", "Set it once. Acme runs it every morning."),
        ("checkmark.seal", "You're all set", "Everything is a tap away."),
    ]

    var body: some View {
        let (symbol, title, detail) = Self.pages[page]
        VStack(spacing: 0) {
            Spacer()
            Image(systemName: symbol)
                .font(.system(size: 64, weight: .regular))
                .foregroundStyle(.tint)
                .frame(height: 96)
            Text(title).font(.title.bold()).padding(.top, 24)
            Text(detail).foregroundStyle(.secondary).multilineTextAlignment(.center).padding(.top, 8).padding(.horizontal, 32)
            Spacer()
            Button {} label: { Text(page == 2 ? "Get started" : "Next").frame(maxWidth: .infinity) }
                .buttonStyle(.borderedProminent)
                .controlSize(.large)
                .padding(.horizontal, 24)
            ZStack {
                Button("Skip") {}.foregroundStyle(.secondary)
                    .opacity(page == 2 ? 0 : 1)
                // The bug Codex finds: the dots were pinned a fixed distance
                // from the bottom, which on a short screen lands on Skip.
                Dots(page: page).offset(y: fixed ? -38 : 0)
            }
            .frame(height: 44)
            .padding(.top, fixed ? 30 : 6)
            .padding(.bottom, 12)
        }
    }
}

struct Dots: View {
    let page: Int

    var body: some View {
        HStack(spacing: 7) {
            ForEach(0..<3) { i in
                Capsule().fill(i == page ? Color.accentColor : Color.secondary.opacity(0.35))
                    .frame(width: i == page ? 18 : 7, height: 7)
            }
        }
    }
}

struct Notifications: View {
    let alert: Bool
    let on: Bool

    var body: some View {
        VStack(spacing: 0) {
            Spacer()
            Image(systemName: on ? "bell.badge.fill" : "bell.badge")
                .font(.system(size: 64))
                .foregroundStyle(.orange)
                .frame(height: 96)
            Text("Stay in the loop").font(.title.bold()).padding(.top, 24)
            Text("Hear when a run finishes or needs you.").foregroundStyle(.secondary).multilineTextAlignment(.center).padding(.top, 8).padding(.horizontal, 32)
            Spacer()
            Group {
                if on {
                    Label("Notifications on", systemImage: "checkmark").frame(maxWidth: .infinity)
                        .padding(.vertical, 14)
                        .foregroundStyle(.green)
                        .background(Color.green.opacity(0.14), in: Capsule())
                } else {
                    Button {} label: { Text("Turn on notifications").frame(maxWidth: .infinity) }
                        .buttonStyle(.borderedProminent)
                        .controlSize(.large)
                }
            }
            .padding(.horizontal, 24)
            Button("Not now") {}.foregroundStyle(.secondary).frame(height: 44).padding(.top, 6).padding(.bottom, 12)
                .opacity(on ? 0 : 1)
        }
        // The permission prompt in the alert style iOS draws it with; an
        // ad-hoc signed app never gets the real one on a Simulator.
        .alert("“Acme” Would Like to Send You Notifications", isPresented: .constant(alert)) {
            Button("Don’t Allow", role: .cancel) {}
            Button("Allow") {}
        } message: {
            Text("Notifications may include alerts, sounds, and icon badges. These can be configured in Settings.")
        }
    }
}
