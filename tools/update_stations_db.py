#!/usr/bin/env python3
"""
Usage:
    python3 tools/update_stations_db.py
"""
import argparse
import json
import os
import re
import sys
import urllib.request

RADIO_BROWSER_URL = (
    "http://de1.api.radio-browser.info/json/stations" "?limit=100000&hidebroken=true"
)

SCRIPT_DIR = os.path.dirname(os.path.abspath(__file__))
REPO_DIR = os.path.dirname(SCRIPT_DIR)
DATA_DIR = os.path.join(REPO_DIR, "data")


def fetch(url, timeout=60):
    with urllib.request.urlopen(url, timeout=timeout) as resp:
        return resp.read()


def make_station(
    name, url, country, codec, bitrate, needs_resolve, lat=None, lon=None, language=None
):
    try:
        bitrate_int = int(bitrate or 0)
    except ValueError:
        bitrate_int = 0
    return {
        "name": name,
        "url": url,
        "country": country or "Unknown",
        "codec": codec or "",
        "bitrate": bitrate_int,
        "needsResolve": needs_resolve,
        "lat": lat,
        "lon": lon,
        "language": language or "",
    }


def fetch_radio_browser():
    print("Fetching radio-browser station list...", file=sys.stderr)
    raw = fetch(RADIO_BROWSER_URL, timeout=120)
    stations = json.loads(raw)
    print(f"  {len(stations)} stations", file=sys.stderr)

    out = []
    for s in stations:
        name = (s.get("name") or "").strip()
        url = s.get("url_resolved") or s.get("url") or ""
        if not name or not url:
            continue
        out.append(
            make_station(
                name,
                url,
                (s.get("country") or "").strip(),
                s.get("codec", ""),
                s.get("bitrate", 0),
                False,
                lat=s.get("geo_lat"),
                lon=s.get("geo_long"),
                language=s.get("language"),
            )
        )
    return out


def slugify(name):
    slug = re.sub(r"[^a-z0-9]+", "_", name.lower()).strip("_")
    return slug or "unknown"


# Maps a country-name variant (lowercased, with a leading "the " already
# stripped - see normalize_country) to one canonical display name, so
# radio-browser's differing conventions (ISO long-form vs. common name,
# "The X" vs "X", old vs. current names, ...) land in the same bucket
# instead of splitting a country's stations across near-duplicate entries.
# Built by inspecting the actual country names radio-browser produced - not
# an attempt at a general-purpose ISO-3166 alias table.
COUNTRY_ALIASES = {
    "bosnia and herzegovina": "Bosnia and Herzegovina",
    "cote d'ivoire": "Ivory Coast",
    "coted ivoire": "Ivory Coast",
    "guinea bissau": "Guinea-Bissau",
    "guinea-bissau": "Guinea-Bissau",
    "myanmar (burma)": "Myanmar",
    "macedonia": "North Macedonia",
    "republic of north macedonia": "North Macedonia",
    "democratic republic of the congo": "DR Congo",
    "dr congo": "DR Congo",
    "democratic peoples republic of korea": "North Korea",
    "republic of korea": "South Korea",
    "syrian arab republic": "Syria",
    "taiwan, republic of china": "Taiwan",
    "united republic of tanzania": "Tanzania",
    "united states of america": "United States",
    "united kingdom of great britain and northern ireland": "United Kingdom",
    "great britain": "United Kingdom",
    "republic of moldova": "Moldova",
    "russian federation": "Russia",
    "philippines": "Philippines",
    "gambia": "Gambia",
    "bahamas": "Bahamas",
    "dominican republic": "Dominican Republic",
    "cocos keeling islands": "Cocos (Keeling) Islands",
    "falkland islands malvinas": "Falkland Islands",
    "holy see": "Vatican City",
    "vatican city": "Vatican City",
    "lao peoples democratic republic": "Laos",
    "turks and caicos islands": "Turks and Caicos Islands",
    "swaziland": "Eswatini",
    "eswatini": "Eswatini",
    "wallis and futuna": "Wallis and Futuna",
    "wallis-futuna islands": "Wallis and Futuna",
    "st. helena": "Saint Helena",
    "ascension and tristan da cunha saint helena": "Saint Helena",
    "st. pierre-miquelon": "Saint Pierre and Miquelon",
    "saint pierre and miquelon": "Saint Pierre and Miquelon",
    "dutch part sint maarten": "Sint Maarten",
    "french part saint martin": "Saint Martin",
    "islamic republic of iran": "Iran",
    "brunei darussalam": "Brunei",
    "east timor": "Timor-Leste",
    "timor leste": "Timor-Leste",
    "federated states of micronesia": "Micronesia",
    "bolivarian republic of venezuela": "Venezuela",
    "state of palestine": "Palestine",
    "palestine": "Palestine",
}


def normalize_country(name):
    name = (name or "Unknown").strip()
    if name.lower().startswith("the "):
        name = name[4:].strip()
    return COUNTRY_ALIASES.get(name.lower(), name)


def dedupe_by_url(stations):
    seen = set()
    deduped = []
    for station in stations:
        key = station["url"].strip().lower()
        if key in seen:
            continue
        seen.add(key)
        deduped.append(station)
    print(
        f"Deduped {len(stations) - len(deduped)} station(s) with a "
        f"repeated stream URL ({len(stations)} -> {len(deduped)})",
        file=sys.stderr,
    )
    return deduped


def write_dataset(all_stations):
    all_stations = dedupe_by_url(all_stations)

    by_country = {}
    for station in all_stations:
        country = normalize_country(station["country"])
        station["country"] = country
        by_country.setdefault(country, []).append(station)
    for stations in by_country.values():
        stations.sort(key=lambda s: s["name"])

    os.makedirs(DATA_DIR, exist_ok=True)
    countries_dir = os.path.join(DATA_DIR, "countries")
    os.makedirs(countries_dir, exist_ok=True)

    country_index = []
    for country in sorted(by_country.keys()):
        stations = by_country[country]
        slug = slugify(country)
        country_index.append(
            {"name": country, "file": slug + ".json", "count": len(stations)}
        )

        # "country" is implied by which file this is, so drop it from the
        # per-station records to avoid repeating it thousands of times.
        trimmed = [{k: v for k, v in s.items() if k != "country"} for s in stations]
        with open(
            os.path.join(countries_dir, slug + ".json"), "w", encoding="utf-8"
        ) as f:
            json.dump(trimmed, f, ensure_ascii=False, indent=2)

    with open(os.path.join(DATA_DIR, "countries.json"), "w", encoding="utf-8") as f:
        json.dump(country_index, f, ensure_ascii=False, indent=2)

    print(f"Wrote {len(by_country)} country files to {countries_dir}", file=sys.stderr)


def main():
    parser = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    parser.parse_args()

    write_dataset(fetch_radio_browser())


if __name__ == "__main__":
    main()
