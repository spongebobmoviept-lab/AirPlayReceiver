fn main() {
    // The icon Explorer and shortcuts show. The running window sets its own from icon.png.
    let mut res = winresource::WindowsResource::new();
    res.set_icon("assets/icon.ico");
    res.compile().expect("couldn't embed the icon");
}
