import SwiftUI

@main
struct DiriPhoneApp: App {
    @UIApplicationDelegateAdaptor(PushDelegate.self) private var push
    @State private var model = AppModel()
    @Environment(\.scenePhase) private var scenePhase

    var body: some Scene {
        WindowGroup {
            Group {
                if !model.hasGateways {
                    ConnectView()
                } else {
                    RootView()
                }
            }
            .environment(model)
            .preferredColorScheme(.dark)
            .tint(Tokens.Ink.clay)
            .onChange(of: scenePhase) { _, phase in model.setActive(phase != .background) }
            .task(id: model.hasGateways) {
                push.model = model
                if model.hasGateways { push.requestAuthorization() }
            }
        }
    }
}
