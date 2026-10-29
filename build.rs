fn main() {
    embed_resource::compile("unmonitor.rc", embed_resource::NONE)
        .manifest_optional()
        .unwrap();
}
