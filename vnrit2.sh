#!/data/data/com.termux/files/usr/bin/bash
# vnrit-manager.sh - Xvfb + 窗口管理器 + vnrit 统一管理脚本
# 用法：./vnrit-manager.sh {start|stop|restart|status} [选项]

set -euo pipefail

# ===== 默认配置 =====
DISPLAY_NUM="${DISPLAY_NUM:-1}"
PORT="${PORT:-8081}"
WIDTH="${WIDTH:-1280}"
HEIGHT="${HEIGHT:-720}"
DEPTH="${DEPTH:-24}"
BITRATE="${BITRATE:-1000}"
FPS="${FPS:-24}"
WM="${WM:-xfce}"            # 可选: xfce, openbox, none
VNIT_BIN="${VNIT_BIN:-vnrit2}"  # 改为你的 vnrit 可执行文件名

# ===== 路径定义（Termux 兼容） =====
: "${TMPDIR:=$PREFIX/tmp}"   # 确保 TMPDIR 有效
PID_DIR="$TMPDIR/vnrit-${DISPLAY_NUM}"
mkdir -p "$PID_DIR"
XVFB_PID_FILE="$PID_DIR/xvfb.pid"
WM_PID_FILE="$PID_DIR/wm.pid"
VNIT_PID_FILE="$PID_DIR/vnit.pid"

# ===== 颜色输出 =====
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m'
info()  { echo -e "${GREEN}[INFO]${NC} $*"; }
warn()  { echo -e "${YELLOW}[WARN]${NC} $*"; }
error() { echo -e "${RED}[ERROR]${NC} $*"; }

# ===== 帮助信息 =====
usage() {
    cat << EOF
用法: $0 {start|stop|restart|status} [选项]

操作:
  start       启动所有服务（默认）
  stop        停止所有服务
  restart     重启所有服务
  status      查看服务状态

选项（可在 start/restart 时使用）:
  -d NUM      显示编号 (默认: 1)
  -p PORT     HTTP端口 (默认: 8081)
  -r WxH      分辨率 (默认: 1920x1080)
  -b KBPS     码率 kbps (默认: 1000)
  -f FPS      帧率 (默认: 24)
  --wm NAME   窗口管理器: xfce, openbox, none (默认: xfce)
  --bin PATH  vnrit 可执行文件路径 (默认: vnrit2)
  -h          显示此帮助

示例:
  $0 start -d 2 -p 9090 -r 1280x720 --wm openbox
  $0 stop
  $0 restart -b 500
EOF
    exit 0
}

# ===== 解析命令行 =====
ACTION="start"
while [[ $# -gt 0 ]]; do
    case $1 in
        start|stop|restart|status) ACTION="$1"; shift ;;
        -d) DISPLAY_NUM="$2"; shift 2 ;;
        -p) PORT="$2"; shift 2 ;;
        -r) IFS='x' read -r WIDTH HEIGHT <<< "$2"; shift 2 ;;
        -b) BITRATE="$2"; shift 2 ;;
        -f) FPS="$2"; shift 2 ;;
        --wm) WM="$2"; shift 2 ;;
        --bin) VNIT_BIN="$2"; shift 2 ;;
        -h|--help) usage ;;
        *) error "未知选项: $1"; usage ;;
    esac
done

# ===== 更新 PID 文件路径（显示号可能变化） =====
PID_DIR="$TMPDIR/vnrit-${DISPLAY_NUM}"
mkdir -p "$PID_DIR"
XVFB_PID_FILE="$PID_DIR/xvfb.pid"
WM_PID_FILE="$PID_DIR/wm.pid"
VNIT_PID_FILE="$PID_DIR/vnit.pid"

# ===== 函数：检查进程是否存活 =====
is_running() {
    local pid_file="$1"
    [[ -f "$pid_file" ]] && kill -0 "$(cat "$pid_file")" 2>/dev/null
}

# ===== 函数：停止所有服务 =====
stop_services() {
    info "正在停止所有服务 (DISPLAY=:${DISPLAY_NUM})..."
    local stopped=0
    
    for pid_file in "$VNIT_PID_FILE" "$WM_PID_FILE" "$XVFB_PID_FILE"; do
        if [[ -f "$pid_file" ]]; then
            local pid=$(cat "$pid_file")
            if kill -0 "$pid" 2>/dev/null; then
                kill "$pid" 2>/dev/null && info "已终止 PID $pid" && stopped=1
            fi
            rm -f "$pid_file"
        fi
    done
    
    # 额外清理进程（按名称）
    pkill -f "Xvfb :${DISPLAY_NUM}" 2>/dev/null && info "已清理残留 Xvfb"
    pkill -f "${VNIT_BIN}.*--display :${DISPLAY_NUM}" 2>/dev/null && info "已清理残留 vnrit"
    if [[ "$WM" == "xfce" ]]; then
        pkill -f "xfce4-session" 2>/dev/null && info "已清理残留 xfce4-session"
    elif [[ "$WM" == "openbox" ]]; then
        pkill -f "openbox.*:${DISPLAY_NUM}" 2>/dev/null && info "已清理残留 openbox"
    fi
    
    [[ $stopped -eq 0 ]] && warn "没有找到运行中的服务"
    info "清理完成"
}

# ===== 函数：检查依赖 =====
check_dependencies() {
    if ! command -v Xvfb &>/dev/null; then
        error "Xvfb 未安装，请运行: pkg install xvfb"
        exit 1
    fi
    if ! command -v "$VNIT_BIN" &>/dev/null; then
        error "vnrit 可执行文件 '$VNIT_BIN' 未找到，请检查路径或设置 --bin"
        exit 1
    fi
    case "$WM" in
        xfce)
            if ! command -v startxfce4 &>/dev/null; then
                error "startxfce4 未安装，请运行: pkg install xfce4"
                exit 1
            fi
            ;;
        openbox)
            if ! command -v openbox &>/dev/null; then
                error "openbox 未安装，请运行: pkg install openbox"
                exit 1
            fi
            ;;
        none)
            warn "未选择窗口管理器，画面将只有黑屏背景"
            ;;
        *)
            error "不支持的窗口管理器: $WM (可选: xfce, openbox, none)"
            exit 1
            ;;
    esac
}

# ===== 函数：启动服务 =====
start_services() {
    # 检查是否已有服务在运行
    if is_running "$XVFB_PID_FILE" || is_running "$VNIT_PID_FILE"; then
        warn "已有服务在 DISPLAY :${DISPLAY_NUM} 上运行，请先停止或换一个显示号"
        status_services
        return 1
    fi

    check_dependencies

    info "启动 Xvfb :${DISPLAY_NUM} (${WIDTH}x${HEIGHT}x${DEPTH})..."
    Xvfb ":${DISPLAY_NUM}" -screen 0 "${WIDTH}x${HEIGHT}x${DEPTH}" -nolisten tcp &
    local xvfb_pid=$!
    echo "$xvfb_pid" > "$XVFB_PID_FILE"
    sleep 2
    
    if ! is_running "$XVFB_PID_FILE"; then
        error "Xvfb 启动失败，请检查日志"
        rm -f "$XVFB_PID_FILE"
        exit 1
    fi
    info "Xvfb 已启动 (PID: $xvfb_pid)"

    export DISPLAY=":${DISPLAY_NUM}"

    # 启动窗口管理器
    case "$WM" in
        xfce)
            info "启动 Xfce4 桌面环境..."
            startxfce4 &
            local wm_pid=$!
            echo "$wm_pid" > "$WM_PID_FILE"
            sleep 4
            ;;
        openbox)
            info "启动 Openbox 窗口管理器..."
            openbox --startup /dev/null &
            local wm_pid=$!
            echo "$wm_pid" > "$WM_PID_FILE"
            sleep 2
            ;;
        none)
            warn "不启动窗口管理器"
            ;;
    esac

    if [[ "$WM" != "none" ]] && ! is_running "$WM_PID_FILE"; then
        error "窗口管理器启动失败，请检查日志"
        stop_services
        exit 1
    fi

    info "启动 ${VNIT_BIN} (端口 ${PORT}, 码率 ${BITRATE}kbps, ${FPS}fps)..."
    "${VNIT_BIN}" --display ":${DISPLAY_NUM}" \
                 --port "${PORT}" \
                 --bitrate "${BITRATE}" \
                 --framerate "${FPS}" &
    local vnit_pid=$!
    echo "$vnit_pid" > "$VNIT_PID_FILE"
    sleep 2

    if ! is_running "$VNIT_PID_FILE"; then
        error "vnrit 启动失败"
        stop_services
        exit 1
    fi

    info "所有服务已成功启动！"
    echo -e "\n🌐 浏览器访问: http://localhost:${PORT}"
    echo "🛑 停止服务: $0 stop"
}

# ===== 函数：状态查看 =====
status_services() {
    echo "服务状态 (DISPLAY :${DISPLAY_NUM}):"
    for pid_file in "$XVFB_PID_FILE" "$WM_PID_FILE" "$VNIT_PID_FILE"; do
        local name=$(basename "$pid_file" .pid)
        if [[ -f "$pid_file" ]]; then
            local pid=$(cat "$pid_file")
            if kill -0 "$pid" 2>/dev/null; then
                echo "  ✅ $name (PID $pid) 运行中"
            else
                echo "  ❌ $name (PID $pid) 已停止"
            fi
        else
            echo "  ⚪ $name 未启动"
        fi
    done
}

# ===== 执行动作 =====
case "$ACTION" in
    start)
        start_services
        ;;
    stop)
        stop_services
        ;;
    restart)
        stop_services
        sleep 1
        start_services
        ;;
    status)
        status_services
        ;;
    *)
        error "未知操作: $ACTION"
        usage
        ;;
esac
