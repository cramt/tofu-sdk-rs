# Iteration 3: rename report -> archive (force_new replace) and omit the meta
# block entirely. The old report.doc.json is deleted; archive.doc.json is created
# with meta = null (an absent single block).
resource "fs_document" "report" {
  name = "archive"

  section {
    heading = "intro"
    body    = "world"
  }
}
