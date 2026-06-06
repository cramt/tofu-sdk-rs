# Iteration 1: create two files.
resource "fs_file" "alpha" {
  name = "alpha"
  content = {
    hello = "world"
    n     = "1"
  }
}

resource "fs_file" "beta" {
  name = "beta"
}
