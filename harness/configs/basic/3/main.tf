# Iteration 3: rename alpha -> gamma. `name` is force_new, so this replaces the
# resource: the prior file (alpha.json) is deleted and gamma.json is created.
resource "fs_file" "alpha" {
  name = "gamma"
  content = {
    hello = "there"
    extra = "yes"
  }
}
