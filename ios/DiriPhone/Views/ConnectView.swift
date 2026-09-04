import SwiftUI
import VisionKit

struct ConnectView: View {
    var onConnected: () -> Void = {}
    @Environment(AppModel.self) private var model
    @State private var link = ""
    @State private var scanning = false
    @State private var connecting = false
    @State private var error: String?

    var body: some View {
        NavigationStack {
            ScrollView {
                VStack(alignment: .leading, spacing: 24) {
                    Image(systemName: "laptopcomputer.and.iphone").font(.system(size: 48)).foregroundStyle(Tokens.Ink.clay)
                    Text("Your projects.\nIn your pocket.").font(.largeTitle.bold())
                    Text("Open Diri on your Mac, then Settings → Phone access. Connect Tailscale on both devices and enable phone access.")
                        .foregroundStyle(.secondary)
                    Button { scanning = true } label: {
                        Label("Scan pairing code", systemImage: "qrcode.viewfinder").frame(maxWidth: .infinity)
                    }
                    .buttonStyle(.borderedProminent).controlSize(.large)
                    .disabled(connecting || !DataScannerViewController.isSupported || !DataScannerViewController.isAvailable)
                    if !DataScannerViewController.isSupported || !DataScannerViewController.isAvailable {
                        Text("Camera scanning isn’t available here. Use a pairing link below, or allow camera access in Settings.")
                            .font(.footnote).foregroundStyle(.secondary)
                    }
                    DisclosureGroup("Or paste a pairing link") {
                        TextField("Pairing link from your Mac", text: $link, axis: .vertical)
                            .textInputAutocapitalization(.never).autocorrectionDisabled().keyboardType(.URL)
                            .textContentType(.none).privacySensitive()
                            .padding(.vertical, 8)
                        Button("Connect") { connect() }.disabled(link.isEmpty || connecting)
                    }
                    if connecting { ProgressView("Connecting to your Mac…") }
                    if let error { Text(error).foregroundStyle(.red) }
                    Text("Keep Diri running and your Mac plugged in with its lid open. Remote sessions are available through this Mac too.")
                        .font(.footnote).foregroundStyle(.secondary)
                }.padding(24)
            }
            .background(Tokens.Ink.background)
            .sheet(isPresented: $scanning) {
                NavigationStack {
                    PairingScanner(onScan: { value in
                        scanning = false
                        link = value
                        connect()
                    }, onFailure: { message in
                        scanning = false
                        error = message
                    })
                    .navigationTitle("Scan your Mac’s code")
                    .navigationBarTitleDisplayMode(.inline)
                    .toolbar { ToolbarItem(placement: .cancellationAction) { Button("Cancel") { scanning = false } } }
                }
            }
        }
    }

    private func connect() {
        guard !connecting else { return }
        connecting = true
        error = nil
        Task {
            do { try await model.connect(link: link); onConnected() }
            catch { self.error = "\(error.localizedDescription) Check that your Mac is awake and Tailscale is connected on both devices." }
            connecting = false
        }
    }
}

private struct PairingScanner: UIViewControllerRepresentable {
    let onScan: (String) -> Void
    let onFailure: (String) -> Void
    func makeCoordinator() -> Coordinator { Coordinator(onScan: onScan, onFailure: onFailure) }
    func makeUIViewController(context: Context) -> DataScannerViewController {
        let scanner = DataScannerViewController(recognizedDataTypes: [.barcode(symbologies: [.qr])],
            qualityLevel: .balanced, recognizesMultipleItems: false, isGuidanceEnabled: true,
            isHighlightingEnabled: true)
        scanner.delegate = context.coordinator
        Task { @MainActor in
            do { try scanner.startScanning() }
            catch { onFailure("Camera couldn’t start. Allow camera access in Settings, or paste a pairing link.") }
        }
        return scanner
    }
    func updateUIViewController(_ scanner: DataScannerViewController, context: Context) {}
    static func dismantleUIViewController(_ scanner: DataScannerViewController, coordinator: Coordinator) { scanner.stopScanning() }

    final class Coordinator: NSObject, DataScannerViewControllerDelegate {
        let onScan: (String) -> Void
        let onFailure: (String) -> Void
        private var delivered = false
        init(onScan: @escaping (String) -> Void, onFailure: @escaping (String) -> Void) {
            self.onScan = onScan
            self.onFailure = onFailure
        }
        func dataScanner(_ scanner: DataScannerViewController, becameUnavailableWithError error: DataScannerViewController.ScanningUnavailable) {
            onFailure("Camera scanning is unavailable. You can still paste the pairing link from your Mac.")
        }
        func dataScanner(_ scanner: DataScannerViewController, didAdd addedItems: [RecognizedItem], allItems: [RecognizedItem]) {
            guard !delivered else { return }
            for item in addedItems {
                if case let .barcode(barcode) = item, let value = barcode.payloadStringValue,
                   DiriClient.Endpoint(enrolmentURL: value) != nil {
                    delivered = true
                    scanner.stopScanning()
                    onScan(value)
                    return
                }
            }
        }
    }
}
