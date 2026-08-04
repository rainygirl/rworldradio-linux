# R World Radio

*[English version](README.md)*

![screenshot](screenshot.png)

Linux 데스크톱용 인터넷 라디오 플레이어입니다. 국가를 고르고 방송국을 고르면 재생됩니다 -
229개국 약 51,000개 방송국 목록이 앱에 함께 들어있습니다. 직접 고른 방송국 외에는
네트워크에 접속하지 않습니다.

Linux Mint XFCE(x86_64)에서 만들고 테스트했습니다.

## 요구 사항

- **Rust 1.88 이상.** Mint/Debian 패키지의 `rustc`는 버전이 낮으므로
  [rustup](https://rustup.rs)으로 설치하세요:

  ```
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
  ```

- 빌드 패키지:

  ```
  sudo apt install build-essential pkg-config libasound2-dev
  ```

- OpenGL이 되는 X11 또는 XWayland 세션. Mint XFCE 설치본에는 이미 있습니다.
- 일본어·중국어·한국어 방송국 이름이 네모가 아니라 글자로 나오게 하려면:
  `sudo apt install fonts-noto-cjk`

## 빌드

```
cargo run --release
```

2코어 / 4GB 정도의 머신에서는 링크 단계에서 메모리가 부족할 수 있습니다. 빌드가 심하게
느려지거나 강제 종료되면 LTO 없이 빌드하세요. 바이너리가 약간 커지는 것 외에는 차이가
없습니다:

```
CARGO_PROFILE_RELEASE_LTO=false CARGO_BUILD_JOBS=2 nice cargo build --release
```

## 설치

```
./install.sh              # 사용자 계정 기준, root 불필요
./install.sh --system     # /usr/local, root 필요
./install.sh --uninstall  # --system으로 설치했다면 --system을 함께 붙이세요
```

앱 메뉴의 **멀티미디어** 아래에 나타나며, 로그아웃이나 재부팅이 필요 없습니다.

사용자 계정 기준 설치는 다음 위치에 씁니다:

```
~/.local/bin/rworldradio
~/.local/share/rworldradio/data/                  방송국 목록
~/.local/share/applications/rworldradio.desktop
~/.local/share/icons/hicolor/<크기>/apps/rworldradio.png
```

방송국 목록은 링크가 아니라 복사되므로, 이 디렉토리를 옮기거나 지워도 설치된 앱은 계속
동작합니다. `--uninstall`은 위 항목 전부를 제거합니다.

`~/.local/bin`이 `PATH`에 없으면 메뉴 항목은 절대 경로를 쓰므로 정상 동작하지만, 셸에서
`rworldradio` 명령은 안 됩니다. 다음으로 추가하세요:

```
export PATH="$HOME/.local/bin:$PATH"
```

## 사용법

- 두 검색창에 입력해 필터링합니다. 229개국 약 51,000개 방송국이라 검색이 이동 수단입니다.
- 국가를 클릭한 뒤 방송국을 더블클릭하면 재생됩니다 - 또는 선택하고 ▶ 버튼을 누르세요.
- ■ 버튼은 재생 중일 때만 나타납니다.
- LED 바는 실제로 재생되고 있는 오디오의 레벨입니다.
- 방송국에 마우스를 올리면 코덱, 비트레이트, 언어, 위치, 스트림 URL이 보입니다.
- 오른쪽 상태 표시에 마우스를 올리면 어떤 방송국 목록을 어디서 읽었는지 보입니다.

## 방송국 목록 최신 상태로 유지하기

```
python3 tools/update_stations_db.py
```

radio-browser의 카탈로그를 가져와 `data/countries.json`과 모든
`data/countries/<slug>.json`을 다시 씁니다. 인터넷 연결이 필요합니다. 갱신한 목록을
설치본에 반영하려면 이후에 `./install.sh`를 다시 실행하세요.

앱 자체는 이 작업을 하지 않습니다 - 함께 배포된 목록만 읽습니다.

## 방송국이 재생되지 않을 때

방송국은 생기고 사라지며, 공개 디렉토리에는 이미 죽은 항목도 적지 않습니다. 문제가 어느
쪽에 있는지는 다음 두 도구로 확인할 수 있습니다:

```
cargo run --release --example probe_stream -- "BBC Radio 4" 5   # 오디오 장치 사용 안 함
cargo run --release --example play_stream  -- "BBC Radio 4" 8   # ALSA로 재생
```

둘 다 방송국 이름(일부만도 됩니다) 또는 URL을 직접 받습니다.

- `probe_stream` 실패 → 방송국이 죽었거나, 지역 차단이거나, 지원하지 않는 형식입니다.
  메시지에 어느 경우인지 나옵니다.
- `probe_stream`은 되는데 `play_stream`이 안 됨 → 문제는 방송국이 아니라 오디오
  출력입니다. PulseAudio나 PipeWire가 실행 중인지 확인하세요.
- 둘 다 되는데 앱에서 소리가 안 남 → 볼륨과 데스크톱이 선택한 출력 장치를 확인하세요.

## 지원하지 않는 것

- **HE-AAC / AAC+ 방송국은 재생되지만 원래보다 둔탁하게 들립니다.** 코어 레이어만
  디코딩하므로 고역대가 빠지고 샘플레이트도 보통 절반입니다.
- **Opus 스트림은 재생되지 않습니다.** Ogg/Vorbis는 됩니다.
- **암호화된 HLS 스트림(`#EXT-X-KEY`)과 fragmented MP4/CMAF 세그먼트는 재생되지
  않습니다.**
- 일부 방송국은 브라우저처럼 보이지 않는 접속을 거부하거나, 자국 내에서만 접속을
  허용합니다.

## 방송국 목록 출처

- [radio-browser](https://www.radio-browser.info/)

## 라이선스

MIT - [LICENSE](LICENSE) 참고.
