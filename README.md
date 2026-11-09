# sheet2dir

`sheet2dir` is a high-performance, asynchronous Rust utility designed to automate document processing, case management, and directory hierarchy generation directly from **Google Sheets**.

It handles complex data normalization—including name parsing, honorific extraction, passport number detection, visa/document validity calculation, and household/family grouping—organizing everything into structured local file layouts with JSON metadata outputs.

---

## Features

* **Async Google Sheets Integration**: Connects via Google Sheets API v4 using Service Account authentication and Tokio asynchronous runtimes.
* **Intelligent Name Parsing Engine**: Automatically strips titles (Mr, Dr, Prof, Inf, etc.), handles standard names, and normalizes inverted CSV formats (`Last, First`).
* **Advanced Passport Extraction**: Employs optimized regex patterns (`OnceLock`) to detect primary passports, multiple additional passports, and explicit inventory quantities.
* **Household & Family Grouping**: Dynamically categorizes records into Single Person, Applicant Family, or Sponsor Family structures based on family IDs and member roles.
* **Automated Filesystem Builder**: Automatically generates structured monthly folders, subdirectories (`apply`, `documents`, `bank`, `epson`, etc.), summary files (`info.txt`), and structured JSON exports (`client_data.json` / `family_data.json`).
* **Resilient I/O Operations**: Features built-in retry mechanisms and post-write verification to ensure reliable file creation even under heavy load.

---

## Project Structure & Output Hierarchy

The utility organizes generated cases by month and category:

```text
YYYY-Babel/
├── [Country]@Full Name PassportID/
│   ├── apply/
│   ├── documents/
│   ├── bank/
│   ├── data/
│   │   └── client_data.json
│   ├── info.txt
│   └── remarks.txt
└── [Country]@Anchor Name . FAMILY Pax N ID/
    ├── ROLE - Member Name PassportID/
    └── family_data.json

```

---

## Prerequisites

* **Rust Toolchain**: Stable Rust (Edition 2021 or later). Cargo package manager.
* **Google Cloud Project**: A Google Cloud service account with permissions to read your target Google Sheet.

---

## Configuration

The application requires specific environment variables to authenticate with Google APIs and locate your target spreadsheet. Create a `.env` file or export them in your shell:

```bash
export SPREADSHEET_ID="your_google_spreadsheet_id_here"
export GOOGLE_APPLICATION_CREDENTIALS="/path/to/your/service_account_key.json"

```

*(Note: You can also use `SERVICE_ACCOUNT_KEY` interchangeably with `GOOGLE_APPLICATION_CREDENTIALS`)*

---

## Installation & Building

1. Clone the repository:
```bash
git clone https://github.com/santobollove/sheet2dir.git
cd sheet2dir

```


2. Build the project in release mode for maximum performance:
```bash
cargo build --release

```



The compiled binary will be available at `target/release/sheet2dir`.

---


## Google Sheet Structure & Formatting Rules

The application reads data sequentially from the designated range (default: `WORK!A2:I`). For successful parsing and folder generation, your Google Sheet **must** follow this exact 9-column layout in order:

| Column | Header Name | Expected Format / Example | Description |
| :---: | :--- | :--- | :--- |
| **A** | `SL` | `1`, `01`, `A-101` | Serial number or tracking identifier. |
| **B** | `Rec. Date` | `2026-03-07`, `15/04/2026` | Date documents were received. Accepts multiple standard date formats. |
| **C** | `Client Name` | `John Doe`, `SMITH, MR JOHN` | Client's full name or CSV-formatted name. Title/honorifics (e.g., `MR`, `DR`) are automatically stripped. |
| **D** | `Passport Summary` | `A12345678`, `A12345678 - 02 Passport` | Primary passport number. Also parses additional passport numbers and explicit quantities. |
| **E** | `Expire Date` | `2030-12-31`, `N/A` | Passport expiry date. Used to compute validity against the received date. |
| **F** | `Country` | `USA`, `CAN` | Target destination or country tag. Prepended to directory names (`USA@...`). |
| **G** | `ROLE` | `SELF`, `HEAD`, `SPONSOR`, `MEMBER` | Applicant's role within a family or case. Defaults to `SELF` if left empty. |
| **H** | `FAMILY ID` | `FAM-102`, `F-88` | Grouping key. Rows sharing the same non-empty `FAMILY ID` are bundled into a single family case folder. |
| **I** | `Remarks` | `Pending medicals`, `VFS done` | Additional notes saved directly into `remarks.txt` within the case directory. |

---

### Example Sheet Row Layout

```text
+----+------------+----------------+------------------+-------------+---------+------+-----------+-------------------+
| A  | B          | C              | D                | E           | F       | G    | H         | I                 |
+----+------------+----------------+------------------+-------------+---------+------+-----------+-------------------+
| 01 | 2026-03-01 | MR JOHN SMITH  | A12345678 - 02 PP| 2031-05-15  | CANADA  | HEAD | FAM-2026  | Urgent Processing |
+----+------------+----------------+------------------+-------------+---------+------+-----------+-------------------+
```

## Usage

Run the executable directly from your terminal. You can specify a custom sheet range as a command-line argument, or let the application prompt you interactively.

### Via CLI Argument

```bash
cargo run -- "WORK!A2:I50"

```

*(If no range is supplied, it defaults to parsing `WORK!A2:I`)*

### Interactive Mode

Simply run:

```bash
cargo run

```

Press `[Enter]` to accept the default range (`WORK!A2:I`), or type a custom range (e.g., `C12:I14`).

---

## Contributing

Contributions, issues, and merge requests are welcome! Please check the [issues page](https://www.google.com/search?q=https://gitlab.com/your-username/sheet2dir/-/issues) for active tasks.

1. Fork the Project
2. Create your Feature Branch (`git checkout -b feature/amazing-feature`)
3. Commit your Changes (`git commit -m 'feat: add amazing feature'`)
4. Push to the Branch (`git push origin feature/amazing-feature`)
5. Open a Merge Request

---

## License

Distributed under the **GNU GENERAL PUBLIC LICENSE v3**. See `LICENSE` for more information.
