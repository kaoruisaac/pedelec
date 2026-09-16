import { writeFile, appendFile, stat } from "node:fs/promises";

export async function writeNote(note: string): Promise<void> {
    // check if the file exists
    const path = ".pedelec-runtime/assets/notes.txt";
    const exists = await stat(path).then(() => true).catch(() => false);
    if (exists) {
        // append to the file
        await appendFile(path, `\n${note}`);
    } else {
        // create the file
        await writeFile(path, note);
    }
}