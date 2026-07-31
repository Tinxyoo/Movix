# docs 目录

存放 README 引用的素材和可视化演示。

| 子目录/文件 | 用途 |
|-------------|------|
| `screenshots/` | 静态截图（PNG）|
| `screenshots/README.md` | 截图清单与重新生成说明 |
| `demo.tape` | vhs 录制脚本（生成 `demo.gif`）|

## 重新生成

```bash
# 静态截图:用 OS 截图工具截,然后保存到对应路径
# macOS:Cmd+Shift+4
# Windows:Snipping Tool / Win+Shift+S
# Linux:gnome-screenshot / flameshot

# 演示动图
brew install vhs && vhs docs/demo.tape
```

## 提交规范

- PNG 用 `oxipng` 压缩后再提交（节省 30-50% 体积）
- GIF 大小控制在 5MB 以内
- 截图前清掉所有 PII（用户名、邮箱、绝对路径）
