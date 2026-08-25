import Foundation
import Testing
@testable import Zeron

struct StudioComposerModelsTests {
    @Test func snapshotEncodesEngineControlTags() throws {
        var snapshot = StudioComposerSnapshot(conversationId: "conversation")
        snapshot.duration = .durationSeconds(6)
        snapshot.selected = [StudioSelectedModel(
            providerId: "provider",
            modelId: "model",
            outputCount: 2,
            controls: ["aspect_ratio": .aspectRatio(width: 16, height: 9)]
        )]

        let object = try #require(
            JSONSerialization.jsonObject(with: JSONEncoder().encode(snapshot)) as? [String: Any]
        )
        let duration = try #require(object["duration"] as? [String: Any])
        #expect(duration["type"] as? String == "duration_seconds")
        let selected = try #require(object["selected"] as? [[String: Any]])
        let controls = try #require(selected.first?["controls"] as? [String: Any])
        let aspect = try #require(controls["aspect_ratio"] as? [String: Any])
        #expect(aspect["type"] as? String == "aspect_ratio")
        #expect(aspect["width"] as? Int == 16)
    }

    @Test func evaluationDecodesTrayAndConflictContract() throws {
        let data = Data(#"""
        {
          "phase":"needs_resolution",
          "mode":"video",
          "send":{"enabled":false,"blockedReason":"missing_required_input"},
          "globals":{"duration":{"type":"duration_seconds","value":6},"durationChoices":[]},
          "models":[{
            "modelId":"video-model","displayName":"Video Model","operation":"image_to_video",
            "outputCount":1,"controls":[],"values":{},"mappedInputs":[],"badge":null
          }],
          "attachments":{"items":[],"accept":{"mimeTypes":["image/jpeg"]},"addEnabled":true},
          "budgets":[],"hints":[{"text":"Needs a start frame","subjects":["video-model"]}],
          "conflicts":[{
            "id":"missing_required_input:video-model::","code":"missing_required_input",
            "severity":"block_send","title":"Video Model needs a start frame",
            "explanation":"Attach a start frame, or remove this model.",
            "subjects":{"modelIds":["video-model"],"assetIds":[],"controlIds":[]},
            "actions":[{"action":{"type":"deselect_incompatible_models","model_ids":["video-model"]},"label":"Remove this model"}]
          }],
          "catalogStale":false,"openPicker":false,"refreshCatalog":false
        }
        """#.utf8)

        let view = try JSONDecoder().decode(StudioComposerEvaluation.self, from: data)
        #expect(view.send.enabled == false)
        #expect(view.models.first?.operation == .imageToVideo)
        #expect(view.attachments.addEnabled)
        #expect(view.conflicts.first?.actions.first?.label == "Remove this model")
        #expect(view.globals.duration == .durationSeconds(6))
    }
}
