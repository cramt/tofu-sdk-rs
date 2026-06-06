# Iteration 1: create a document with a single `meta` block and two `section`
# blocks (nested-block syntax).
resource "fs_document" "report" {
  name = "report"

  meta {
    author = "alice"
    note   = "draft"
  }

  section {
    heading = "intro"
    body    = "hello"
  }

  section {
    heading = "body"
  }
}
