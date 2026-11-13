import PveDevCore
import SwiftUI

struct RootPartitionField: View {
    @Binding var text: String
    var body: some View {
        DisclosureGroup("Root partition selection") {
            VStack(alignment: .leading, spacing: 8) {
                Text("For multiple ext4 partitions, specify the intended root's GPT partition UUID. Leave blank for a supported unambiguous layout.")
                    .font(.caption).foregroundStyle(.secondary)
                TextField("Root partition UUID (optional)", text: $text)
                if !RootPartitionSelection(text: text).isValid {
                    Text("Enter a complete partition UUID, or leave the field blank.").font(.caption).foregroundStyle(.red)
                }
            }.padding(.top, 6)
        }
    }
}
