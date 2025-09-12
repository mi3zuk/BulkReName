fn main() {
    let mut res = winres::WindowsResource::new();
    res.set_icon("BulkReName.ico"); // .ico ファイルを指定
    res.compile().unwrap();
}