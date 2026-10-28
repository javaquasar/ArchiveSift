fn main() -> std::io::Result<()> {
    archive_sift_scraper_api::serve_stdio(archive_sift_demo_plugin::execute)
}
