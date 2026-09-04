import AVFoundation
import SwiftUI
import VisionKit

struct ConnectView: View {
  var onConnected: () -> Void = {}
  @Environment(AppModel.self) private var model
  @Environment(\.openURL) private var openURL
  @Environment(\.accessibilityReduceMotion) private var reduceMotion
  @State private var step = SetupStep.mac
  @State private var link = ""
  @State private var scanning = false
  @State private var connecting = false
  @State private var error: String?
  @State private var cameraDenied = false
  @State private var connectionTask: Task<Void, Never>?

  private enum SetupStep: Int, CaseIterable {
    case mac, phone, pair
    var title: String {
      switch self {
      case .mac: "Prepare your Mac"
      case .phone: "Connect your iPhone"
      case .pair: "Make the connection"
      }
    }
  }

  var body: some View {
    NavigationStack {
      ScrollView {
        VStack(alignment: .leading, spacing: 24) {
          Image(systemName: "laptopcomputer.and.iphone").font(.largeTitle).foregroundStyle(
            Tokens.Ink.clay
          )
          .accessibilityHidden(true)
          Text("Your projects. In your pocket.").font(.largeTitle.bold())
          Text("Step \(step.rawValue + 1) of 3 · \(step.title)").font(.headline)
          Group {
            switch step {
            case .mac: macSetup
            case .phone: phoneSetup
            case .pair: pairing
            }
          }
          .id(step)
          .transition(.opacity)
          Text(
            "Keep Diri running and your Mac plugged in with its lid open. Your phone can use Wi-Fi or mobile data."
          )
          .font(.footnote).foregroundStyle(.secondary)
          DisclosureGroup("Privacy & connection") {
            Text(
              "Tailscale is a separate app that encrypts the connection between your devices. Diri never asks for your Tailscale password. Your pairing key stays in this iPhone’s Keychain. Scanning happens on-device; camera images are not uploaded. Anyone with your pairing code and access to your private network can control your sessions. Turn phone access off on your Mac to revoke it."
            )
            .font(.footnote).foregroundStyle(.secondary).padding(.top, 8)
          }
        }.padding(24)
      }
      .background(Tokens.Ink.background)
      .animation(reduceMotion ? nil : .easeInOut(duration: 0.2), value: step)
      .toolbar {
        if step != .mac {
          ToolbarItem(placement: .topBarLeading) {
            Button("Back", systemImage: "chevron.left") {
              step = step == .pair ? .phone : .mac
              error = nil
            }.disabled(connecting)
          }
        }
      }
      .onAppear { if model.endpoint != nil { step = .pair } }
      .onDisappear { connectionTask?.cancel() }
      .sheet(isPresented: $scanning) {
        NavigationStack {
          PairingScanner(
            onScan: { value in
              scanning = false
              link = value
              connect()
            },
            onFailure: { message in
              scanning = false
              error = message
            }
          )
          .navigationTitle("Scan your Mac’s code")
          .navigationBarTitleDisplayMode(.inline)
          .toolbar {
            ToolbarItem(placement: .cancellationAction) { Button("Cancel") { scanning = false } }
          }
        }
      }
    }
  }

  private var macSetup: some View {
    VStack(alignment: .leading, spacing: 20) {
      Text(
        "On your Mac, open Diri → Settings → Phone access. Choose Check this Mac and follow the Tailscale setup steps."
      )
      Text(
        "Tailscale gives your devices a private connection. You don’t need to change your router or type any commands."
      ).foregroundStyle(.secondary)
      Button("My Mac is ready") { step = .phone }
        .buttonStyle(.borderedProminent).controlSize(.large)
      Button("Already set up? Scan a code") { step = .pair }
    }
  }

  private var phoneSetup: some View {
    VStack(alignment: .leading, spacing: 20) {
      Text(
        "Install Tailscale on this iPhone, then open it and sign in with the same account you used on your Mac."
      )
      Link(destination: URL(string: "https://apps.apple.com/app/tailscale/id1470499037")!) {
        Label("Get or open Tailscale", systemImage: "arrow.up.right.square")
      }.buttonStyle(.bordered).controlSize(.large)
      Text(
        "Allow the VPN configuration when iOS asks. Once Tailscale says Connected, come back here. Leave exit nodes and other advanced settings unchanged."
      ).foregroundStyle(.secondary)
      Button("Tailscale is connected — continue") { step = .pair }
        .buttonStyle(.borderedProminent).controlSize(.large)
      Text(
        "We’ll verify that Diri can reach your Mac after you scan its code. iOS doesn’t let Diri inspect another app’s sign-in."
      )
      .font(.footnote).foregroundStyle(.secondary)
    }
  }

  private var pairing: some View {
    VStack(alignment: .leading, spacing: 20) {
      Text("On your Mac, enable phone access in Diri’s settings. Scan the code that appears there.")
      Button(action: startScanning) {
        Label("Scan pairing code", systemImage: "qrcode.viewfinder").frame(maxWidth: .infinity)
      }
      .buttonStyle(.borderedProminent).controlSize(.large)
      .disabled(connecting || !DataScannerViewController.isSupported)
      if !DataScannerViewController.isSupported {
        Text(
          "This device doesn’t support the scanner. Copy the pairing link on your Mac and paste it below."
        )
        .font(.footnote).foregroundStyle(.secondary)
      }
      DisclosureGroup("Or paste a pairing link") {
        TextField("Pairing link from your Mac", text: $link, axis: .vertical)
          .textInputAutocapitalization(.never).autocorrectionDisabled().keyboardType(.URL)
          .textContentType(.none).privacySensitive()
          .padding(.vertical, 8)
        Button("Connect") { connect() }.disabled(link.isEmpty || connecting)
      }
      if connecting {
        ProgressView("Connecting to your Mac…")
        Button("Cancel connection") { connectionTask?.cancel() }
      }
      if let error {
        Text(error).foregroundStyle(.red).accessibilityIdentifier("pairing-error")
      }
      if cameraDenied {
        Button("Open camera settings") {
          openURL(URL(string: UIApplication.openSettingsURLString)!)
        }
      }
      DisclosureGroup("Can’t connect?") {
        Text(
          "Check Tailscale says Connected on both devices and uses the same account. On the Mac, keep Diri open with phone access on. If access was turned off or Diri restarted, scan the new code. A work-managed Tailscale network may need your administrator to allow this connection."
        )
        .font(.footnote).foregroundStyle(.secondary).padding(.top, 8)
      }
    }
  }

  private func startScanning() {
    error = nil
    cameraDenied = false
    connectionTask = Task {
      let allowed = await AVCaptureDevice.requestAccess(for: .video)
      guard !Task.isCancelled else { return }
      guard allowed else {
        cameraDenied = true
        error = "Allow camera access to scan, or paste the pairing link below."
        return
      }
      guard DataScannerViewController.isAvailable else {
        error = "The camera isn’t available right now. Try again or paste the pairing link."
        return
      }
      scanning = true
    }
  }

  private func connect() {
    guard !connecting else { return }
    connecting = true
    error = nil
    connectionTask = Task {
      defer { connecting = false }
      do {
        try await model.connect(link: link)
        onConnected()
      } catch { if !Task.isCancelled { self.error = PairingHelp.message(for: error) } }
    }
  }
}

enum PairingHelp {
  static func message(for error: Error) -> String {
    switch error {
    case DiriClient.Failure.unauthorized:
      "This code is no longer valid. Scan the current code in Diri → Settings → Phone access on your Mac."
    case DiriClient.Failure.daemonUnreachable:
      "Your Mac answered, but its session engine isn’t ready. Open Diri on the Mac and try again."
    case DiriClient.Failure.http(400, _):
      "Use the pairing code or link from Diri → Settings → Phone access on your Mac."
    case DiriClient.Failure.malformed:
      "This doesn’t look like a compatible Diri connection. Update Diri on both devices and scan a fresh code."
    default:
      "We couldn’t reach Diri. Keep your Mac awake with phone access on, and connect Tailscale on both devices using the same account. Then try again."
    }
  }
}

private struct PairingScanner: UIViewControllerRepresentable {
  let onScan: (String) -> Void
  let onFailure: (String) -> Void
  func makeCoordinator() -> Coordinator { Coordinator(onScan: onScan, onFailure: onFailure) }
  func makeUIViewController(context: Context) -> DataScannerViewController {
    let scanner = DataScannerViewController(
      recognizedDataTypes: [.barcode(symbologies: [.qr])],
      qualityLevel: .balanced, recognizesMultipleItems: false, isGuidanceEnabled: true,
      isHighlightingEnabled: true)
    scanner.delegate = context.coordinator
    Task { @MainActor in
      do { try scanner.startScanning() } catch {
        onFailure(
          "Camera couldn’t start. Allow camera access in Settings, or paste a pairing link.")
      }
    }
    return scanner
  }
  func updateUIViewController(_ scanner: DataScannerViewController, context: Context) {}
  static func dismantleUIViewController(
    _ scanner: DataScannerViewController, coordinator: Coordinator
  ) { scanner.stopScanning() }

  final class Coordinator: NSObject, DataScannerViewControllerDelegate {
    let onScan: (String) -> Void
    let onFailure: (String) -> Void
    private var delivered = false
    init(onScan: @escaping (String) -> Void, onFailure: @escaping (String) -> Void) {
      self.onScan = onScan
      self.onFailure = onFailure
    }
    func dataScanner(
      _ scanner: DataScannerViewController,
      becameUnavailableWithError error: DataScannerViewController.ScanningUnavailable
    ) {
      onFailure(
        "Camera scanning is unavailable. You can still paste the pairing link from your Mac.")
    }
    func dataScanner(
      _ scanner: DataScannerViewController, didAdd addedItems: [RecognizedItem],
      allItems: [RecognizedItem]
    ) {
      guard !delivered else { return }
      for item in addedItems {
        if case .barcode(let barcode) = item, let value = barcode.payloadStringValue,
          DiriClient.Endpoint(enrolmentURL: value) != nil
        {
          delivered = true
          scanner.stopScanning()
          onScan(value)
          return
        }
      }
    }
  }
}
