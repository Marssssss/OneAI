// Voice input via the macOS Speech framework (SFSpeechRecognizer +
// AVAudioEngine). Targets macOS 13. Real-time, partial-result dictation: the
// mic button in the InputBar starts/stops it; recognized text fills the input
// field.
//
// Locale: prefer zh-CN (the app's primary audience), fall back to the system
// locale if a zh recognizer isn't available. Each recognized utterance is
// accumulated (committed) so a multi-sentence dictation builds up in the field
// rather than being overwritten by the latest utterance.
//
// Notes on macOS-specific API shape vs iOS:
// - The auth request is `SFSpeechRecognizer.requestAuthorization(_:)` on macOS
//   (iOS spells it `requestSpeechAuthorization`).
// - `SFSpeechRecognizerDelegate` inherits NSObjectProtocol, so this class
//   inherits NSObject; the delegate callback is nonisolated (it fires on a
//   background queue) and hops to the main actor to mutate @Published state.

import SwiftUI
import Speech
import AVFoundation

@MainActor
final class SpeechRecognizer: NSObject, ObservableObject {
    /// Live dictation transcript (committed utterances + the in-progress one).
    @Published private(set) var transcript: String = ""
    /// Actively listening.
    @Published private(set) var isRunning: Bool = false
    /// A recognizer + mic are available (set false on auth denial / no mic).
    @Published private(set) var available: Bool = true

    private var recognizer: SFSpeechRecognizer?
    private var request: SFSpeechAudioBufferRecognitionRequest?
    private var task: SFSpeechRecognitionTask?
    private let engine = AVAudioEngine()

    // Accumulated finalized utterances (so a new utterance doesn't drop the
    // previous one from the transcript).
    private var committed: String = ""
    private var currentUtterance: String = ""

    override init() {
        // Prefer zh-CN; fall back to system locale if unavailable.
        let loc = SFSpeechRecognizer.supportedLocales().contains(where: { $0.identifier == "zh_CN" })
            ? Locale(identifier: "zh_CN") : Locale.current
        let r = SFSpeechRecognizer(locale: loc) ?? SFSpeechRecognizer()
        super.init()
        recognizer = r
        available = r?.isAvailable ?? false
        r?.delegate = self
    }

    // MARK: - Lifecycle

    func start() {
        guard !isRunning else { return }
        guard let recognizer, recognizer.isAvailable else { available = false; return }

        // macOS: `requestAuthorization(_:)` (not the iOS `requestSpeechAuthorization`).
        SFSpeechRecognizer.requestAuthorization { [weak self] status in
            DispatchQueue.main.async {
                guard status == .authorized else { self?.available = false; return }
                self?.beginCapture()
            }
        }
    }

    private func beginCapture() {
        guard let recognizer else { return }
        committed = ""
        currentUtterance = ""
        transcript = ""

        let req = SFSpeechAudioBufferRecognitionRequest()
        req.shouldReportPartialResults = true
        request = req

        let inputNode = engine.inputNode
        let fmt = inputNode.outputFormat(forBus: 0)
        inputNode.removeTap(onBus: 0)
        inputNode.installTap(onBus: 0, bufferSize: 1024, format: fmt) { buffer, _ in
            req.append(buffer)
        }

        engine.prepare()
        do { try engine.start() }
        catch { available = false; stop(); return }

        task = recognizer.recognitionTask(with: req) { [weak self] result, error in
            DispatchQueue.main.async {
                guard let self else { return }
                if let result { self.handle(result: result) }
                if error != nil || (result?.isFinal ?? false) { self.stop() }
            }
        }
        isRunning = true
    }

    func stop() {
        engine.inputNode.removeTap(onBus: 0)
        engine.stop()
        request?.endAudio()
        task?.cancel()
        task = nil
        request = nil
        isRunning = false
    }

    // MARK: - Transcript accumulation

    private func handle(result: SFSpeechRecognitionResult) {
        let f = result.bestTranscription.formattedString
        // Heuristic utterance boundary: the running transcription of the
        // current utterance grows by extension. If the new text is NOT a
        // prefix-extension of the last, a new utterance began — commit the old.
        if currentUtterance.isEmpty || f.hasPrefix(currentUtterance) {
            currentUtterance = f
        } else {
            if !currentUtterance.isEmpty {
                committed += (committed.isEmpty ? "" : " ") + currentUtterance
            }
            currentUtterance = f
        }
        let sep = committed.isEmpty || currentUtterance.isEmpty ? "" : " "
        transcript = committed + sep + currentUtterance
    }
}

extension SpeechRecognizer: SFSpeechRecognizerDelegate {
    // Fires on a background queue — nonisolated so it satisfies the protocol,
    // then hops to the main actor to mutate @Published state.
    nonisolated func speechRecognizer(_ speechRecognizer: SFSpeechRecognizer,
                                      availabilityDidChange available: Bool) {
        Task { @MainActor in self.available = available }
    }
}
